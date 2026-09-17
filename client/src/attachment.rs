//! The attachment cache (specification §29, `docs/fcp.md` §6).
//!
//! Downloads are content-addressed: the file on disk is named after its SHA-256,
//! which is also the `ETag` the server sends. Two consequences the UI benefits
//! from directly:
//!
//! * the same attachment attached to three messages costs one copy on disk;
//! * an interrupted download resumes with a `Range` request instead of starting
//!   over, and an interrupted upload resumes from the chunk bitmap the server
//!   reports at `GET /attachments/:id/status`.
//!
//! The cache is bounded by a byte cap and evicts least-recently-used, unpinned
//! blobs. Every blob is verified against its own name when it is read back, so a
//! damaged file is refetched rather than handed to the user.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures_util::StreamExt;
use tokio::io::AsyncWriteExt;

use crate::api::{AttachmentDto, FcpClient, UploadComplete, UploadStatus};
use crate::database::ClientDatabase;
use crate::error::{ClientError, ClientResult};
use crate::util::{now_rfc3339, sha256_hex};

/// The default cache cap: 1 GiB (`settings.storage.max_cache_bytes` overrides it).
pub const DEFAULT_CACHE_LIMIT: u64 = 1024 * 1024 * 1024;

/// Bytes below which the one-shot multipart upload is used instead of the
/// chunked protocol.
pub const SIMPLE_UPLOAD_LIMIT: u64 = 4 * 1024 * 1024;

/// A blob that lives in the cache, or was just written to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedBlob {
    /// The SHA-256 of the contents — also the file name.
    pub sha256: String,
    /// Size in bytes.
    pub size_bytes: u64,
    /// Where the bytes are.
    pub path: PathBuf,
    /// Whether this read was served without touching the network.
    pub from_cache: bool,
}

impl CachedBlob {
    /// Read the blob back into memory (small attachments, previews, tests).
    pub fn read(&self) -> ClientResult<Vec<u8>> {
        Ok(std::fs::read(&self.path)?)
    }
}

/// The content-addressed attachment cache.
#[derive(Debug, Clone)]
pub struct AttachmentCache {
    root: PathBuf,
    db: Arc<ClientDatabase>,
    cap_bytes: u64,
}

impl AttachmentCache {
    /// Open (creating if needed) a cache rooted at `root`, holding at most
    /// `cap_bytes`.
    ///
    /// The cap is taken literally — `0` means "cache nothing", which is a legal
    /// choice for a device with no room to spare. Blobs are never evicted by a
    /// write; [`AttachmentCache::evict_to_cap`] is what trims the cache, and the
    /// downloaded blob is returned before it runs.
    pub fn new(root: impl Into<PathBuf>, db: Arc<ClientDatabase>, cap_bytes: u64) -> Self {
        AttachmentCache {
            root: root.into(),
            db,
            cap_bytes,
        }
    }

    /// A cache with the default 1 GiB cap.
    pub fn with_default_cap(root: impl Into<PathBuf>, db: Arc<ClientDatabase>) -> Self {
        AttachmentCache::new(root, db, DEFAULT_CACHE_LIMIT)
    }

    /// The directory the blobs live in.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The configured cap in bytes.
    pub fn cap_bytes(&self) -> u64 {
        self.cap_bytes
    }

    /// Change the cap (the §52 storage setting).
    pub fn set_cap_bytes(&mut self, cap_bytes: u64) {
        self.cap_bytes = cap_bytes;
    }

    /// Where a blob with this digest lives. The two-character prefix keeps the
    /// directory from holding hundreds of thousands of entries.
    pub fn blob_path(&self, sha256: &str) -> PathBuf {
        let prefix = sha256.get(0..2).unwrap_or("00");
        self.root.join("objects").join(prefix).join(sha256)
    }

    /// Where an in-progress download is assembled.
    pub fn partial_path(&self, attachment_id: i64) -> PathBuf {
        self.root.join("tmp").join(format!("{attachment_id}.part"))
    }

    /// Write bytes into the cache under their digest, returning the blob.
    pub async fn put_bytes(&self, bytes: &[u8]) -> ClientResult<CachedBlob> {
        let sha256 = sha256_hex(bytes);
        let path = self.blob_path(&sha256);
        if !path.exists() {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            // Write to a sibling temp file and rename, so a crash never leaves a
            // half-written blob under a valid digest.
            let temp = path.with_extension("tmp");
            tokio::fs::write(&temp, bytes).await?;
            tokio::fs::rename(&temp, &path).await?;
        }
        self.record(&sha256, bytes.len() as u64, &path).await?;
        Ok(CachedBlob {
            sha256,
            size_bytes: bytes.len() as u64,
            path,
            from_cache: false,
        })
    }

    async fn record(&self, sha256: &str, size_bytes: u64, path: &Path) -> ClientResult<()> {
        sqlx::query(
            "INSERT INTO blob_cache (sha256, size_bytes, path, last_access, access_seq, pinned)
             VALUES (?, ?, ?, ?, (SELECT COALESCE(MAX(access_seq), 0) + 1 FROM blob_cache), 0)
             ON CONFLICT(sha256) DO UPDATE SET
                 size_bytes = excluded.size_bytes,
                 path = excluded.path,
                 last_access = excluded.last_access,
                 access_seq = excluded.access_seq",
        )
        .bind(sha256)
        .bind(size_bytes as i64)
        .bind(path.to_string_lossy().to_string())
        .bind(now_rfc3339())
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    /// Bump the LRU stamp of a blob. The counter is monotonic, so two touches in
    /// the same second still have a defined order.
    async fn touch(&self, sha256: &str) -> ClientResult<()> {
        sqlx::query(
            "UPDATE blob_cache
             SET last_access = ?, access_seq = (SELECT COALESCE(MAX(access_seq), 0) + 1 FROM blob_cache)
             WHERE sha256 = ?",
        )
        .bind(now_rfc3339())
        .bind(sha256)
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    /// Look a blob up by digest, verifying that the file still hashes to its own
    /// name.
    ///
    /// A file that fails verification — a truncated write, a corrupted disk, a
    /// user poking around in the cache directory — is deleted and reported as a
    /// miss, so the caller refetches it. Verifying costs one read of the file;
    /// that is a deliberate trade for never handing over corrupt bytes.
    pub async fn get(&self, sha256: &str) -> ClientResult<Option<CachedBlob>> {
        let row = sqlx::query("SELECT size_bytes, path FROM blob_cache WHERE sha256 = ?")
            .bind(sha256)
            .fetch_optional(self.db.pool())
            .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        use sqlx::Row;
        let size_bytes: i64 = row.try_get("size_bytes")?;
        let path = self.blob_path(sha256);
        if !path.exists() {
            sqlx::query("DELETE FROM blob_cache WHERE sha256 = ?")
                .bind(sha256)
                .execute(self.db.pool())
                .await?;
            return Ok(None);
        }
        let bytes = tokio::fs::read(&path).await?;
        if sha256_hex(&bytes) != sha256 {
            tracing::warn!(sha256, "a cached attachment failed its digest check; dropping it");
            let _ = tokio::fs::remove_file(&path).await;
            sqlx::query("DELETE FROM blob_cache WHERE sha256 = ?")
                .bind(sha256)
                .execute(self.db.pool())
                .await?;
            return Ok(None);
        }
        self.touch(sha256).await?;
        Ok(Some(CachedBlob {
            sha256: sha256.to_string(),
            size_bytes: size_bytes.max(0) as u64,
            path,
            from_cache: true,
        }))
    }

    /// Whether a blob is present and intact.
    pub async fn contains(&self, sha256: &str) -> ClientResult<bool> {
        Ok(self.get(sha256).await?.is_some())
    }

    /// Pin (or unpin) a blob so eviction leaves it alone.
    pub async fn set_pinned(&self, sha256: &str, pinned: bool) -> ClientResult<()> {
        sqlx::query("UPDATE blob_cache SET pinned = ? WHERE sha256 = ?")
            .bind(i64::from(pinned))
            .bind(sha256)
            .execute(self.db.pool())
            .await?;
        Ok(())
    }

    /// Forget one blob.
    pub async fn remove(&self, sha256: &str) -> ClientResult<bool> {
        let path = self.blob_path(sha256);
        let existed = path.exists();
        let _ = tokio::fs::remove_file(&path).await;
        sqlx::query("DELETE FROM blob_cache WHERE sha256 = ?")
            .bind(sha256)
            .execute(self.db.pool())
            .await?;
        Ok(existed)
    }

    /// How many bytes the cache holds.
    pub async fn total_size(&self) -> ClientResult<u64> {
        use sqlx::Row;
        let row = sqlx::query("SELECT COALESCE(SUM(size_bytes), 0) AS total FROM blob_cache")
            .fetch_one(self.db.pool())
            .await?;
        let total: i64 = row.try_get("total")?;
        Ok(total.max(0) as u64)
    }

    /// How many blobs the cache holds.
    pub async fn count(&self) -> ClientResult<i64> {
        use sqlx::Row;
        let row = sqlx::query("SELECT COUNT(*) AS n FROM blob_cache")
            .fetch_one(self.db.pool())
            .await?;
        Ok(row.try_get("n")?)
    }

    /// Evict least-recently-used, unpinned blobs until the cache is under its cap.
    ///
    /// Returns the number of bytes freed.
    pub async fn evict_to_cap(&self) -> ClientResult<u64> {
        let mut total = self.total_size().await?;
        if total <= self.cap_bytes {
            return Ok(0);
        }
        use sqlx::Row;
        let rows = sqlx::query(
            "SELECT sha256, size_bytes FROM blob_cache WHERE pinned = 0 ORDER BY access_seq ASC",
        )
        .fetch_all(self.db.pool())
        .await?;

        let mut freed = 0u64;
        for row in rows {
            if total <= self.cap_bytes {
                break;
            }
            let sha256: String = row.try_get("sha256")?;
            let size: i64 = row.try_get("size_bytes")?;
            let size = size.max(0) as u64;
            let path = self.blob_path(&sha256);
            let _ = tokio::fs::remove_file(&path).await;
            sqlx::query("DELETE FROM blob_cache WHERE sha256 = ?")
                .bind(&sha256)
                .execute(self.db.pool())
                .await?;
            total = total.saturating_sub(size);
            freed += size;
        }
        Ok(freed)
    }

    /// Delete every blob (the "clear cache" button, and logout-with-wipe).
    pub async fn clear(&self) -> ClientResult<u64> {
        let freed = self.total_size().await?;
        sqlx::query("DELETE FROM blob_cache")
            .execute(self.db.pool())
            .await?;
        let objects = self.root.join("objects");
        if objects.exists() {
            let _ = tokio::fs::remove_dir_all(&objects).await;
        }
        let tmp = self.root.join("tmp");
        if tmp.exists() {
            let _ = tokio::fs::remove_dir_all(&tmp).await;
        }
        Ok(freed)
    }

    // -- download ----------------------------------------------------------

    /// Fetch an attachment into the cache, resuming a partial download.
    ///
    /// When `expected_sha256` is known (the API reports it, and the `ETag` is the
    /// same value) a cache hit returns without a single byte over the wire.
    pub async fn download(
        &self,
        client: &FcpClient,
        attachment_id: i64,
        expected_sha256: Option<&str>,
    ) -> ClientResult<CachedBlob> {
        if let Some(sha) = expected_sha256 {
            if let Some(blob) = self.get(sha).await? {
                return Ok(blob);
            }
        }

        let partial = self.partial_path(attachment_id);
        if let Some(parent) = partial.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let existing = tokio::fs::metadata(&partial)
            .await
            .map(|meta| meta.len())
            .unwrap_or(0);

        let response = if existing > 0 {
            client
                .download_attachment(attachment_id, Some((existing, None)))
                .await?
        } else {
            client.download_attachment(attachment_id, None).await?
        };

        let resumed = response.status().as_u16() == 206;
        let mut file = if resumed && existing > 0 {
            tokio::fs::OpenOptions::new()
                .append(true)
                .open(&partial)
                .await?
        } else {
            tokio::fs::File::create(&partial).await?
        };

        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|err| ClientError::Network(err.to_string()))?;
            file.write_all(&chunk).await?;
        }
        file.flush().await?;
        drop(file);

        let bytes = tokio::fs::read(&partial).await?;
        let sha256 = sha256_hex(&bytes);

        if let Some(expected) = expected_sha256 {
            if !expected.is_empty() && expected != sha256 {
                let _ = tokio::fs::remove_file(&partial).await;
                return Err(ClientError::cache(format!(
                    "attachment {attachment_id} hashed to {sha256}, not the {expected} the server announced"
                )));
            }
        }

        let path = self.blob_path(&sha256);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        tokio::fs::rename(&partial, &path).await?;
        self.record(&sha256, bytes.len() as u64, &path).await?;
        self.evict_to_cap().await?;

        Ok(CachedBlob {
            sha256,
            size_bytes: bytes.len() as u64,
            path,
            from_cache: false,
        })
    }

    // -- upload ------------------------------------------------------------

    /// Upload a small file with one multipart request (§6).
    pub async fn upload_bytes(
        &self,
        client: &FcpClient,
        filename: &str,
        content_type: &str,
        bytes: Vec<u8>,
    ) -> ClientResult<AttachmentDto> {
        client
            .upload_attachment(filename, content_type, bytes)
            .await
    }

    /// Upload a file, resuming an interrupted upload.
    ///
    /// `GET /attachments/:id/status` reports the chunk bitmap the server holds,
    /// so a crash mid-upload only costs the chunks that were actually missing —
    /// which is the whole point of the chunked protocol in §6.
    pub async fn upload_resumable(
        &self,
        client: &FcpClient,
        filename: &str,
        content_type: &str,
        bytes: &[u8],
    ) -> ClientResult<UploadComplete> {
        let init = client
            .init_upload(filename, content_type, bytes.len() as u64)
            .await?;
        let status = match client.upload_status(init.attachment_id).await {
            Ok(status) => status,
            // A brand-new upload has no status yet on some servers.
            Err(ClientError::Api(ref api)) if api.status == 404 => UploadStatus {
                attachment_id: init.attachment_id,
                chunk_size: init.chunk_size,
                received: Vec::new(),
                complete: false,
                size_bytes: bytes.len() as u64,
            },
            Err(err) => return Err(err),
        };

        let chunk_size = if status.chunk_size > 0 {
            status.chunk_size
        } else if init.chunk_size > 0 {
            init.chunk_size
        } else {
            bytes.len().max(1) as u64
        };
        let held: HashSet<u64> = status.received.iter().copied().collect();
        let step = chunk_size.max(1) as usize;

        for (index, chunk) in bytes.chunks(step).enumerate() {
            let index = index as u64;
            if held.contains(&index) {
                continue;
            }
            client
                .upload_chunk(init.attachment_id, index, chunk.to_vec())
                .await?;
        }

        let sha256 = sha256_hex(bytes);
        client.complete_upload(init.attachment_id, &sha256).await
    }

    /// Upload a file, choosing the simple or the resumable path by size.
    pub async fn upload_smart(
        &self,
        client: &FcpClient,
        filename: &str,
        content_type: &str,
        bytes: Vec<u8>,
    ) -> ClientResult<UploadComplete> {
        if bytes.len() as u64 <= SIMPLE_UPLOAD_LIMIT {
            let dto = client
                .upload_attachment(filename, content_type, bytes)
                .await?;
            return Ok(UploadComplete {
                id: dto.id,
                filename: dto.filename,
                content_type: dto.content_type,
                size_bytes: dto.size_bytes,
                sha256: dto.sha256,
            });
        }
        self.upload_resumable(client, filename, content_type, &bytes)
            .await
    }

    /// Upload a file from disk.
    pub async fn upload_file(
        &self,
        client: &FcpClient,
        path: &Path,
        content_type: &str,
    ) -> ClientResult<UploadComplete> {
        let bytes = tokio::fs::read(path).await?;
        let filename = path
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| "attachment".to_string());
        self.upload_smart(client, &filename, content_type, bytes)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::RetryPolicy;
    use crate::testutil::{MockResponse, MockServer, TempDir};

    async fn fixture(cap: u64) -> (TempDir, AttachmentCache, MockServer, FcpClient) {
        let dir = TempDir::new().expect("temp dir");
        let db = Arc::new(
            ClientDatabase::open(dir.path().join("cache.db"))
                .await
                .expect("open"),
        );
        let cache = AttachmentCache::new(dir.path().join("blobs"), db, cap);
        let server = MockServer::start().await;
        let client = FcpClient::with_retry(
            server.fcp_base_url(),
            Arc::new(crate::api::MemoryTokenStore::new()),
            RetryPolicy::none(),
        )
        .expect("client");
        client
            .set_access_token("t", std::time::Duration::from_secs(60))
            .await;
        (dir, cache, server, client)
    }

    /// A route that serves `body`, honouring `Range` like a real blob store.
    fn serve_body(server: &MockServer, body: &'static [u8]) {
        server.route("GET", "/api/v1/client/attachments/9", move |req, _n| {
            if let Some(range) = req.header("range") {
                let start = range
                    .trim_start_matches("bytes=")
                    .split('-')
                    .next()
                    .and_then(|v| v.parse::<usize>().ok())
                    .unwrap_or(0);
                let slice = body.get(start..).unwrap_or(&[]);
                MockResponse::Full {
                    status: 206,
                    headers: vec![
                        ("Content-Type".into(), "application/octet-stream".into()),
                        ("Content-Range".into(), format!("bytes {start}-{}/{}", body.len() - 1, body.len())),
                    ],
                    body: slice.to_vec(),
                }
            } else {
                MockResponse::Full {
                    status: 200,
                    headers: vec![("Content-Type".into(), "application/octet-stream".into())],
                    body: body.to_vec(),
                }
            }
        });
    }

    #[tokio::test]
    async fn put_and_get_round_trip() {
        let (_dir, cache, _server, _client) = fixture(DEFAULT_CACHE_LIMIT).await;
        let blob = cache.put_bytes(b"hello attachment").await.expect("put");
        assert_eq!(blob.size_bytes, 16);
        assert_eq!(blob.sha256, sha256_hex(b"hello attachment"));
        assert!(blob.path.exists());
        let again = cache.get(&blob.sha256).await.expect("get").expect("hit");
        assert!(again.from_cache);
        assert_eq!(again.read().expect("read"), b"hello attachment");
        assert!(cache.get("not a digest").await.expect("miss").is_none());
    }

    #[tokio::test]
    async fn a_digest_is_the_file_name_and_two_copies_cost_one_file() {
        let (_dir, cache, _server, _client) = fixture(DEFAULT_CACHE_LIMIT).await;
        let first = cache.put_bytes(b"same bytes").await.expect("first");
        let second = cache.put_bytes(b"same bytes").await.expect("second");
        assert_eq!(first.sha256, second.sha256);
        assert_eq!(first.path, second.path);
        assert_eq!(cache.count().await.expect("count"), 1);
    }

    #[tokio::test]
    async fn a_cache_hit_avoids_a_second_download() {
        let (_dir, cache, server, client) = fixture(DEFAULT_CACHE_LIMIT).await;
        let body: &[u8] = b"the bytes of the invoice";
        serve_body(&server, body);
        let expected = sha256_hex(body);

        let first = cache
            .download(&client, 9, Some(&expected))
            .await
            .expect("first download");
        assert!(!first.from_cache);
        assert_eq!(server.count_for("/api/v1/client/attachments/9"), 1);

        let second = cache
            .download(&client, 9, Some(&expected))
            .await
            .expect("second download");
        assert!(second.from_cache);
        assert_eq!(
            server.count_for("/api/v1/client/attachments/9"),
            1,
            "a cache hit must not touch the network"
        );
        assert_eq!(second.read().expect("read"), body);
    }

    #[tokio::test]
    async fn a_partial_download_resumes_with_a_range_request() {
        let (_dir, cache, server, client) = fixture(DEFAULT_CACHE_LIMIT).await;
        let body: &[u8] = b"0123456789abcdef";
        serve_body(&server, body);

        // Leave 6 bytes of a previous attempt behind.
        let partial = cache.partial_path(9);
        std::fs::create_dir_all(partial.parent().expect("parent")).expect("mkdir");
        std::fs::write(&partial, b"012345").expect("seed partial");

        let blob = cache.download(&client, 9, None).await.expect("resume");
        assert_eq!(blob.read().expect("read"), body);

        let request = server.requests_for("/api/v1/client/attachments/9").remove(0);
        assert_eq!(request.header("range"), Some("bytes=6-"));
    }

    #[tokio::test]
    async fn a_server_that_ignores_the_range_restarts_cleanly() {
        let (_dir, cache, server, client) = fixture(DEFAULT_CACHE_LIMIT).await;
        let body: &[u8] = b"abcdefghij";
        // Always answer 200 with the whole body, even for a range request.
        server.route("GET", "/api/v1/client/attachments/9", move |_req, _n| {
            MockResponse::Full {
                status: 200,
                headers: vec![],
                body: body.to_vec(),
            }
        });
        let partial = cache.partial_path(9);
        std::fs::create_dir_all(partial.parent().expect("parent")).expect("mkdir");
        std::fs::write(&partial, b"abc").expect("seed partial");

        let blob = cache.download(&client, 9, None).await.expect("download");
        assert_eq!(blob.read().expect("read"), body, "no duplicated prefix");
    }

    #[tokio::test]
    async fn a_corrupted_cache_entry_is_detected_and_refetched() {
        let (_dir, cache, server, client) = fixture(DEFAULT_CACHE_LIMIT).await;
        let body: &[u8] = b"trustworthy bytes";
        serve_body(&server, body);
        let sha = sha256_hex(body);

        let blob = cache.download(&client, 9, Some(&sha)).await.expect("download");
        // Corrupt the file without touching its name.
        std::fs::write(&blob.path, b"tampered").expect("corrupt");

        assert!(
            cache.get(&sha).await.expect("get").is_none(),
            "a blob that does not hash to its name is a miss"
        );
        assert!(
            cache.count().await.expect("count") == 0,
            "the damaged blob must be dropped, not left behind"
        );

        let refetched = cache.download(&client, 9, Some(&sha)).await.expect("refetch");
        assert!(!refetched.from_cache);
        assert_eq!(refetched.read().expect("read"), body);
        assert_eq!(server.count_for("/api/v1/client/attachments/9"), 2);
    }

    #[tokio::test]
    async fn a_download_whose_digest_does_not_match_is_refused() {
        let (_dir, cache, server, client) = fixture(DEFAULT_CACHE_LIMIT).await;
        serve_body(&server, b"actual contents");
        let err = cache
            .download(&client, 9, Some(&sha256_hex(b"expected contents")))
            .await
            .expect_err("must refuse");
        assert!(matches!(err, ClientError::Cache(_)), "got {err:?}");
        assert_eq!(cache.count().await.expect("count"), 0);
        assert!(
            !cache.partial_path(9).exists(),
            "the bad download must not be left in tmp"
        );
    }

    #[tokio::test]
    async fn a_missing_file_is_reported_as_a_miss() {
        let (_dir, cache, _server, _client) = fixture(DEFAULT_CACHE_LIMIT).await;
        let blob = cache.put_bytes(b"gone soon").await.expect("put");
        std::fs::remove_file(&blob.path).expect("remove");
        assert!(cache.get(&blob.sha256).await.expect("get").is_none());
        assert_eq!(cache.count().await.expect("count"), 0, "the index self-heals");
    }

    #[tokio::test]
    async fn eviction_respects_the_cap_and_drops_the_least_recently_used() {
        let (_dir, cache, _server, _client) = fixture(1000).await;
        // 400 bytes each; a 1000-byte cap holds two.
        let a = cache.put_bytes(&vec![b'a'; 400]).await.expect("a");
        let b = cache.put_bytes(&vec![b'b'; 400]).await.expect("b");
        let c = cache.put_bytes(&vec![b'c'; 400]).await.expect("c");

        // Touch a and c so b is the least recently used.
        cache.get(&a.sha256).await.expect("touch a");
        cache.get(&c.sha256).await.expect("touch c");

        let freed = cache.evict_to_cap().await.expect("evict");
        assert_eq!(freed, 400);
        assert!(cache.total_size().await.expect("size") <= 1000);
        assert!(cache.get(&b.sha256).await.expect("b").is_none(), "b was LRU");
        assert!(cache.get(&a.sha256).await.expect("a").is_some());
        assert!(cache.get(&c.sha256).await.expect("c").is_some());
    }

    #[tokio::test]
    async fn a_pinned_blob_survives_eviction() {
        let (_dir, cache, _server, _client) = fixture(1000).await;
        let pinned = cache.put_bytes(&vec![b'p'; 400]).await.expect("pinned");
        cache.set_pinned(&pinned.sha256, true).await.expect("pin");
        let old = cache.put_bytes(&vec![b'o'; 400]).await.expect("old");
        cache.put_bytes(&vec![b'n'; 400]).await.expect("new");
        cache.evict_to_cap().await.expect("evict");

        assert!(
            cache.get(&pinned.sha256).await.expect("pinned").is_some(),
            "a pinned attachment is never evicted"
        );
        assert!(cache.get(&old.sha256).await.expect("old").is_none());
    }

    #[tokio::test]
    async fn clearing_the_cache_removes_the_files() {
        let (_dir, cache, _server, _client) = fixture(DEFAULT_CACHE_LIMIT).await;
        let blob = cache.put_bytes(b"delete me").await.expect("put");
        let freed = cache.clear().await.expect("clear");
        assert_eq!(freed, 9);
        assert!(!blob.path.exists());
        assert_eq!(cache.count().await.expect("count"), 0);
    }

    #[tokio::test]
    async fn removing_one_blob_leaves_the_others() {
        let (_dir, cache, _server, _client) = fixture(DEFAULT_CACHE_LIMIT).await;
        let a = cache.put_bytes(b"a").await.expect("a");
        let b = cache.put_bytes(b"b").await.expect("b");
        assert!(cache.remove(&a.sha256).await.expect("remove"));
        assert!(!cache.remove(&a.sha256).await.expect("remove again"));
        assert!(cache.get(&b.sha256).await.expect("b").is_some());
    }

    #[tokio::test]
    async fn a_chunked_upload_only_sends_the_missing_chunks() {
        let (_dir, cache, server, client) = fixture(DEFAULT_CACHE_LIMIT).await;
        server.json_route(
            "POST",
            "/api/v1/client/attachments/init",
            200,
            r#"{"attachment_id":77,"chunk_size":4,"upload_token":"ut"}"#,
        );
        // The server already holds chunks 0 and 2.
        server.json_route(
            "GET",
            "/api/v1/client/attachments/77/status",
            200,
            r#"{"attachment_id":77,"chunk_size":4,"received":[0,2],"complete":false,"size_bytes":12}"#,
        );
        server.route("PUT", "/api/v1/client/attachments/77/chunk", |_req, _n| {
            MockResponse::json_status(200, "{}")
        });
        server.json_route(
            "POST",
            "/api/v1/client/attachments/77/complete",
            200,
            r#"{"id":77,"filename":"a.bin","content_type":"application/octet-stream","size_bytes":12,"sha256":"x"}"#,
        );

        let bytes = b"abcdefghijkl"; // three 4-byte chunks
        let done = cache
            .upload_resumable(&client, "a.bin", "application/octet-stream", bytes)
            .await
            .expect("upload");
        assert_eq!(done.id, 77);

        let chunks = server.requests_for("/api/v1/client/attachments/77/chunk");
        let indexes: Vec<String> = chunks
            .iter()
            .map(|r| r.query_params().get("index").cloned().unwrap_or_default())
            .collect();
        assert_eq!(indexes, vec!["1"], "only the missing chunk is uploaded");

        let complete = server
            .requests_for("/api/v1/client/attachments/77/complete")
            .remove(0);
        assert_eq!(complete.json()["sha256"], sha256_hex(bytes));
    }

    #[tokio::test]
    async fn a_fresh_upload_sends_every_chunk_when_there_is_no_status() {
        let (_dir, cache, server, client) = fixture(DEFAULT_CACHE_LIMIT).await;
        server.json_route(
            "POST",
            "/api/v1/client/attachments/init",
            200,
            r#"{"attachment_id":77,"chunk_size":4,"upload_token":"ut"}"#,
        );
        server.error_route("GET", "/api/v1/client/attachments/77/status", 404, "not_found", "new");
        server.route("PUT", "/api/v1/client/attachments/77/chunk", |_req, _n| {
            MockResponse::json_status(200, "{}")
        });
        server.json_route(
            "POST",
            "/api/v1/client/attachments/77/complete",
            200,
            r#"{"id":77,"filename":"a.bin","content_type":"application/octet-stream","size_bytes":12}"#,
        );

        cache
            .upload_resumable(&client, "a.bin", "application/octet-stream", b"abcdefghijkl")
            .await
            .expect("upload");
        assert_eq!(server.count_for("/api/v1/client/attachments/77/chunk"), 3);
    }

    #[tokio::test]
    async fn a_small_file_takes_the_simple_path() {
        let (_dir, cache, server, client) = fixture(DEFAULT_CACHE_LIMIT).await;
        server.json_route(
            "POST",
            "/api/v1/client/attachments",
            200,
            r#"{"id":11,"filename":"note.txt","content_type":"text/plain","size_bytes":5}"#,
        );
        let done = cache
            .upload_smart(&client, "note.txt", "text/plain", b"hello".to_vec())
            .await
            .expect("upload");
        assert_eq!(done.id, 11);
        assert_eq!(server.count_for("/api/v1/client/attachments/init"), 0);
    }

    #[tokio::test]
    async fn a_large_file_takes_the_chunked_path() {
        let (_dir, cache, server, client) = fixture(DEFAULT_CACHE_LIMIT).await;
        server.json_route(
            "POST",
            "/api/v1/client/attachments/init",
            200,
            r#"{"attachment_id":88,"chunk_size":1048576,"upload_token":"ut"}"#,
        );
        server.error_route("GET", "/api/v1/client/attachments/88/status", 404, "not_found", "new");
        server.route("PUT", "/api/v1/client/attachments/88/chunk", |_req, _n| {
            MockResponse::json_status(200, "{}")
        });
        server.json_route(
            "POST",
            "/api/v1/client/attachments/88/complete",
            200,
            r#"{"id":88,"size_bytes":5242880}"#,
        );
        let big = vec![7u8; 5 * 1024 * 1024];
        let done = cache
            .upload_smart(&client, "big.bin", "application/octet-stream", big)
            .await
            .expect("upload");
        assert_eq!(done.id, 88);
        assert_eq!(server.count_for("/api/v1/client/attachments/88/chunk"), 5);
    }

    #[tokio::test]
    async fn a_complete_upload_is_never_re_uploaded() {
        let (_dir, cache, server, client) = fixture(DEFAULT_CACHE_LIMIT).await;
        server.json_route(
            "POST",
            "/api/v1/client/attachments/init",
            200,
            r#"{"attachment_id":77,"chunk_size":4,"upload_token":"ut"}"#,
        );
        server.json_route(
            "GET",
            "/api/v1/client/attachments/77/status",
            200,
            r#"{"attachment_id":77,"chunk_size":4,"received":[0,1,2],"complete":true,"size_bytes":12}"#,
        );
        server.json_route(
            "POST",
            "/api/v1/client/attachments/77/complete",
            200,
            r#"{"id":77,"size_bytes":12}"#,
        );
        cache
            .upload_resumable(&client, "a.bin", "application/octet-stream", b"abcdefghijkl")
            .await
            .expect("upload");
        assert_eq!(server.count_for("/api/v1/client/attachments/77/chunk"), 0);
    }

    #[tokio::test]
    async fn the_paths_are_content_addressed() {
        let (_dir, cache, _server, _client) = fixture(DEFAULT_CACHE_LIMIT).await;
        let sha = sha256_hex(b"x");
        let path = cache.blob_path(&sha);
        assert!(path.ends_with(&sha));
        assert!(path.parent().expect("parent").ends_with(&sha[0..2]));
        assert!(cache.partial_path(9).ends_with("9.part"));
    }
}
