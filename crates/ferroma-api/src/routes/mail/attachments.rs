//! `docs/api.md` §5.4 and `docs/fcp.md` §6 — attachments.
//!
//! # Content addressing
//!
//! Blobs live in [`ferroma_storage::AttachmentStore`], which names each file after its
//! SHA-256. That buys two things the protocol relies on: two identical files uploaded
//! twice occupy one blob, and `ETag` can be the digest itself, so a repeated download
//! never re-transfers bytes.
//!
//! # Range and ETag
//!
//! `GET /attachments/:id` supports `Range` and answers `206 Partial Content` with the
//! right `Content-Range`, plus a `304 Not Modified` when the client already holds the
//! digest. That is what makes a resumed download of a 2 GB archive cost only the gap.
//!
//! # Chunked upload
//!
//! `POST /client/attachments/init` reserves an attachment row and hands back a
//! `upload_token`; `PUT …/:id/chunk?index=N` accepts the chunks in any order and is
//! idempotent per index (the same index twice replaces the bytes rather than
//! appending); `GET …/:id/status` reports the gaps so a crashed client uploads only
//! what is missing; `POST …/:id/complete` verifies the digest before the attachment
//! becomes usable. A digest mismatch is a `409 conflict` and the blob is discarded.

use axum::body::Body;
use axum::extract::{Multipart, Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use ferroma_core::{AttachmentId, FerromaError};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::ApiError;
use crate::extract::AuthUser;
use crate::routes::mail::ownership::owned_attachment;
use crate::routes::mail::shapes::AttachmentResponse;
use crate::state::{AppState, UploadSession};

/// The upload an `init` call reserved.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UploadInitResponse {
    /// The reserved attachment row.
    pub attachment_id: i64,
    /// Bytes per chunk, except the last.
    pub chunk_size: u64,
    /// The opaque token that authorises the chunk calls.
    pub upload_token: String,
}

/// What `GET …/status` reports (`docs/fcp.md` §6).
///
/// `received` is the list of chunk indexes the server holds, which is what lets a
/// resuming client send only the gap instead of the whole file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UploadStatusResponse {
    /// Which attachment is being uploaded.
    pub attachment_id: i64,
    /// The declared total size.
    pub size_bytes: u64,
    /// Bytes per chunk.
    pub chunk_size: u64,
    /// How many chunks the size implies.
    pub chunk_count: u64,
    /// The indexes already received, ascending.
    pub received: Vec<u64>,
    /// Whether every chunk is present, so `complete` will succeed.
    pub complete: bool,
}

/// The `POST …/complete` body.
#[derive(Debug, Clone, Deserialize)]
pub struct CompleteUploadRequest {
    /// The SHA-256 of the whole file, lower-case hex.
    pub sha256: String,
}

/// What an upload answers with once it is complete.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachmentMetaResponse {
    /// The attachment row id.
    pub id: i64,
    /// The file name.
    pub filename: Option<String>,
    /// The MIME type.
    pub content_type: String,
    /// Size in bytes.
    pub size_bytes: i64,
    /// Lower-case hex SHA-256 of the bytes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    /// Whether the part is inline.
    #[serde(default)]
    pub is_inline: bool,
    /// The `Content-ID`, for inline parts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_id: Option<String>,
}

/// `POST /attachments` — `multipart/form-data`, streamed into the blob store.
///
/// The upload creates an *unattached* row: `message_id` is a placeholder because the
/// schema requires one, and the send path re-points the row at the real message. A
/// row that is never attached is removed by `POST /storage/gc`.
pub async fn upload_attachment(
    State(state): State<AppState>,
    user: AuthUser,
    mut multipart: Multipart,
) -> Result<(StatusCode, Json<AttachmentMetaResponse>), ApiError> {
    let mut filename = None;
    let mut content_type = None;
    let mut bytes: Option<Vec<u8>> = None;

    while let Some(field) = multipart.next_field().await? {
        let name = field.name().unwrap_or_default().to_string();
        if name == "file" {
            filename = field.file_name().map(str::to_string);
            content_type = field.content_type().map(str::to_string);
            let data = field.bytes().await?;
            if data.len() as u64 > state.config.limits.max_attachment_size {
                return Err(ApiError::new(FerromaError::LimitExceeded(format!(
                    "attachment is {} bytes, limit is {}",
                    data.len(),
                    state.config.limits.max_attachment_size
                ))));
            }
            bytes = Some(data.to_vec());
        } else if name == "filename" {
            filename = Some(field.text().await.unwrap_or_default());
        } else if name == "content_type" {
            content_type = Some(field.text().await.unwrap_or_default());
        }
    }

    let Some(bytes) = bytes else {
        return Err(ApiError::new(FerromaError::Invalid(
            "the multipart body must carry a `file` field".to_string(),
        )));
    };

    let filename = filename
        .map(|name| sanitize_filename(&name))
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "attachment".to_string());
    let content_type = content_type
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "application/octet-stream".to_string());

    let blob = state.attachments.store(&bytes)?;
    let stub = crate::state::MessageStub::ensure(&state, user.user_id()).await?;
    let row = state
        .repos
        .attachments
        .insert(
            stub.message_id,
            ferroma_storage::repository::NewAttachment {
                filename: Some(filename),
                content_type,
                size_bytes: blob.size as i64,
                storage_path: blob.path,
                content_id: None,
                is_inline: false,
                checksum_sha256: Some(blob.sha256.clone()),
            },
        )
        .await?;

    Ok((
        StatusCode::CREATED,
        Json(AttachmentMetaResponse {
            id: row.id,
            filename: row.filename.clone(),
            content_type: row.content_type.clone(),
            size_bytes: row.size_bytes,
            sha256: row.checksum_sha256.clone(),
            is_inline: row.is_inline,
            content_id: row.content_id.clone(),
        }),
    ))
}

/// `GET /attachments/:id/meta`
pub async fn attachment_meta(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
) -> Result<Json<AttachmentMetaResponse>, ApiError> {
    let row = owned_attachment(&state.repos, AttachmentId::new(id), user.user_id()).await?;
    Ok(Json(AttachmentMetaResponse {
        id: row.id,
        filename: row.filename.clone(),
        content_type: row.content_type.clone(),
        size_bytes: row.size_bytes,
        sha256: row.checksum_sha256.clone(),
        is_inline: row.is_inline,
        content_id: row.content_id.clone(),
    }))
}

/// `GET /attachments/:id` — streams the bytes, honouring `Range` and `ETag`.
pub async fn download_attachment(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let row = owned_attachment(&state.repos, AttachmentId::new(id), user.user_id()).await?;

    let etag = row
        .checksum_sha256
        .clone()
        .unwrap_or_else(|| format!("\"att-{}\"", row.id));
    let etag_value = format!("\"{}\"", etag.trim_matches('"'));

    if let Some(inm) = headers.get(header::IF_NONE_MATCH).and_then(|v| v.to_str().ok()) {
        if inm.split(',').any(|candidate| candidate.trim() == etag_value) {
            let mut response = StatusCode::NOT_MODIFIED.into_response();
            insert_header(&mut response, header::ETAG, &etag_value);
            return Ok(response);
        }
    }

    let total = row.size_bytes.max(0) as u64;
    let range = headers
        .get(header::RANGE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| parse_range(value, total));

    let mut response = match range {
        Some((start, end)) => {
            let length = (end - start + 1) as usize;
            let bytes = state.attachments.read_range(&row.storage_path, start, length)?;
            let mut response = (StatusCode::PARTIAL_CONTENT, Body::from(bytes)).into_response();
            insert_header(
                &mut response,
                header::CONTENT_RANGE,
                &format!("bytes {start}-{end}/{total}"),
            );
            response
        }
        None => {
            let bytes = state.attachments.read(&row.storage_path)?;
            (StatusCode::OK, Body::from(bytes)).into_response()
        }
    };

    let content_type = if row.content_type.trim().is_empty() {
        "application/octet-stream".to_string()
    } else {
        row.content_type.clone()
    };
    insert_header(&mut response, header::CONTENT_TYPE, &content_type);
    insert_header(&mut response, header::ETAG, &etag_value);
    insert_header(&mut response, header::ACCEPT_RANGES, "bytes");
    insert_header(
        &mut response,
        header::CONTENT_DISPOSITION,
        &content_disposition(row.filename.as_deref()),
    );
    Ok(response)
}

/// `DELETE /attachments/:id` — only while still unreferenced by a real message.
pub async fn delete_attachment(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
) -> Result<StatusCode, ApiError> {
    let row = owned_attachment(&state.repos, AttachmentId::new(id), user.user_id()).await?;
    let removed = state
        .repos
        .attachments
        .delete(AttachmentId::new(row.id))
        .await?;
    if removed {
        // The blob is content-addressed, so a second row may point at it; `gc` is what
        // removes an unreferenced blob, not this endpoint.
        let still_referenced = state
            .repos
            .attachments
            .referenced_paths()
            .await
            .unwrap_or_default()
            .iter()
            .any(|path| path == &row.storage_path);
        if !still_referenced {
            if let Err(err) = state.attachments.delete(&row.storage_path) {
                tracing::warn!(attachment_id = row.id, error = %err, "blob could not be removed");
            }
        }
    }
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /client/attachments/init`
pub async fn init_upload(
    State(state): State<AppState>,
    user: AuthUser,
    Json(request): Json<UploadInitRequest>,
) -> Result<(StatusCode, Json<UploadInitResponse>), ApiError> {
    if request.size_bytes > state.config.limits.max_attachment_size {
        return Err(ApiError::new(FerromaError::LimitExceeded(format!(
            "attachment is {} bytes, limit is {}",
            request.size_bytes, state.config.limits.max_attachment_size
        ))));
    }
    if request.size_bytes == 0 {
        return Err(ApiError::new(FerromaError::Invalid(
            "size_bytes must be greater than zero".to_string(),
        )));
    }

    // The row is reserved now so the client has a stable id to upload against; it
    // becomes usable only once `complete` verifies the digest.
    let stub = crate::state::MessageStub::ensure(&state, user.user_id()).await?;
    let row = state
        .repos
        .attachments
        .insert(
            stub.message_id,
            ferroma_storage::repository::NewAttachment {
                filename: Some(sanitize_filename(&request.filename)),
                content_type: request
                    .content_type
                    .clone()
                    .unwrap_or_else(|| "application/octet-stream".to_string()),
                size_bytes: request.size_bytes as i64,
                storage_path: String::new(),
                content_id: None,
                is_inline: false,
                checksum_sha256: None,
            },
        )
        .await?;

    let chunk_size = state.config.client.attachment_chunk_size.max(1);
    let token = new_upload_token();
    state.uploads.insert(
        token.clone(),
        UploadSession {
            user_id: user.user_id(),
            filename: row.filename.clone().unwrap_or_default(),
            content_type: row.content_type.clone(),
            size_bytes: request.size_bytes,
            chunk_size,
            attachment_id: Some(row.id),
            chunks: std::collections::HashMap::new(),
            received: std::collections::BTreeSet::new(),
            created_at: chrono::Utc::now(),
        },
    );

    Ok((
        StatusCode::CREATED,
        Json(UploadInitResponse {
            attachment_id: row.id,
            chunk_size,
            upload_token: token,
        }),
    ))
}

/// The `POST /client/attachments/init` body (`docs/fcp.md` §6).
#[derive(Debug, Clone, Deserialize)]
pub struct UploadInitRequest {
    /// The file name the recipient will see.
    pub filename: String,
    /// The MIME type.
    pub content_type: Option<String>,
    /// The total size in bytes.
    pub size_bytes: u64,
}

/// The `?index=N` of a chunk call.
#[derive(Debug, Clone, Copy, Deserialize)]
pub struct ChunkQuery {
    /// The zero-based chunk index.
    pub index: u64,
}

/// `PUT /client/attachments/:id/chunk?index=N`
pub async fn upload_chunk(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
    Query(query): Query<ChunkQuery>,
    body: axum::body::Bytes,
) -> Result<StatusCode, ApiError> {
    // Ownership and existence, before anything is buffered.
    owned_attachment(&state.repos, AttachmentId::new(id), user.user_id()).await?;

    let Some(session) = state.uploads.find_for_attachment(id, user.user_id()) else {
        return Err(ApiError::new(FerromaError::NotFound(
            "no upload session for this attachment".to_string(),
        )));
    };

    if query.index >= session.expected_chunks() {
        return Err(ApiError::new(FerromaError::Invalid(format!(
            "chunk {} is beyond the end of a {} byte upload",
            query.index, session.size_bytes
        ))));
    }
    if body.len() as u64 > session.chunk_size {
        return Err(ApiError::new(FerromaError::LimitExceeded(format!(
            "chunk is {} bytes, the chunk size is {}",
            body.len(),
            session.chunk_size
        ))));
    }

    state
        .uploads
        .put_chunk_for_attachment(id, query.index, body.to_vec());
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /client/attachments/:id/status`
pub async fn upload_status(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
) -> Result<Json<UploadStatusResponse>, ApiError> {
    owned_attachment(&state.repos, AttachmentId::new(id), user.user_id()).await?;
    let session = state
        .uploads
        .find_for_attachment(id, user.user_id())
        .ok_or_else(|| {
            ApiError::new(FerromaError::NotFound(
                "no upload session for this attachment".to_string(),
            ))
        })?;

    Ok(Json(UploadStatusResponse {
        attachment_id: id,
        size_bytes: session.size_bytes,
        chunk_size: session.chunk_size,
        chunk_count: session.expected_chunks(),
        received: session.received.iter().copied().collect(),
        complete: session.missing_chunks().is_empty(),
    }))
}

/// `POST /client/attachments/:id/complete`
pub async fn complete_upload(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
    Json(request): Json<CompleteUploadRequest>,
) -> Result<Json<AttachmentMetaResponse>, ApiError> {
    owned_attachment(&state.repos, AttachmentId::new(id), user.user_id()).await?;

    let Some((token, session)) = state.uploads.take_for_attachment(id, user.user_id()) else {
        return Err(ApiError::new(FerromaError::NotFound(
            "no upload session for this attachment".to_string(),
        )));
    };

    let missing = session.missing_chunks();
    if !missing.is_empty() {
        // Put the session back: the client may still finish it.
        state.uploads.insert(token, session);
        return Err(ApiError::new(FerromaError::Conflict(format!(
            "{} chunk(s) are still missing",
            missing.len()
        ))));
    }

    let bytes = session.assemble()?;
    if bytes.len() as u64 != session.size_bytes {
        return Err(ApiError::new(FerromaError::Conflict(format!(
            "assembled {} bytes but the upload declared {}",
            bytes.len(),
            session.size_bytes
        ))));
    }

    let digest = hex_lower(&Sha256::digest(&bytes));
    if !digest.eq_ignore_ascii_case(request.sha256.trim()) {
        // The upload is discarded, which is what `docs/fcp.md` §6 promises.
        return Err(ApiError::new(FerromaError::Conflict(
            "the uploaded bytes do not match the declared sha256".to_string(),
        )));
    }

    let blob = state.attachments.store(&bytes)?;

    // The reserved row carries no bytes yet, so it is replaced by the finished one.
    // `ferroma-storage` exposes insert/delete rather than a mutation, which keeps this
    // crate free of raw SQL — and the row id changes, so the response reports the new
    // one.
    let existing = owned_attachment(&state.repos, AttachmentId::new(id), user.user_id()).await?;
    let stored = state
        .repos
        .attachments
        .insert(
            ferroma_core::MessageId::new(existing.message_id),
            ferroma_storage::repository::NewAttachment {
                filename: Some(session.filename.clone()),
                content_type: session.content_type.clone(),
                size_bytes: blob.size as i64,
                storage_path: blob.path.clone(),
                content_id: None,
                is_inline: false,
                checksum_sha256: Some(blob.sha256.clone()),
            },
        )
        .await?;
    state
        .repos
        .attachments
        .delete(AttachmentId::new(id))
        .await?;

    Ok(Json(AttachmentMetaResponse {
        id: stored.id,
        filename: stored.filename.clone(),
        content_type: stored.content_type.clone(),
        size_bytes: blob.size as i64,
        sha256: Some(blob.sha256),
        is_inline: false,
        content_id: None,
    }))
}

/// A fresh upload token. Opaque and unguessable; it authorises nothing on its own —
/// the attachment id plus ownership is what a chunk call is actually checked against.
pub fn new_upload_token() -> String {
    format!("up_{}", uuid::Uuid::new_v4().simple())
}

/// A file name that is safe to put in a `Content-Disposition` header and on disk.
pub fn sanitize_filename(raw: &str) -> String {
    let base = raw
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(raw)
        .trim()
        .trim_matches('.');
    let cleaned: String = base
        .chars()
        .map(|ch| match ch {
            '"' => '\'',
            '\r' | '\n' | '\t' => ' ',
            ch if ch.is_control() => '_',
            ch => ch,
        })
        .collect();
    let cleaned = cleaned.trim().to_string();
    if cleaned.len() > 200 {
        cleaned.chars().take(200).collect()
    } else {
        cleaned
    }
}

/// A `Content-Disposition` value that cannot break out of its quoting.
fn content_disposition(filename: Option<&str>) -> String {
    match filename.map(sanitize_filename).filter(|name| !name.is_empty()) {
        Some(name) => format!("attachment; filename=\"{name}\""),
        None => "attachment".to_string(),
    }
}

/// Parse a single-range `Range: bytes=start-end` header.
///
/// Returns `None` for a header this endpoint does not honour — a multi-range request,
/// a malformed one, or a range that starts past the end. Answering with the whole
/// body is the RFC-permitted fallback and is always safe.
pub fn parse_range(raw: &str, total: u64) -> Option<(u64, u64)> {
    let spec = raw.trim().strip_prefix("bytes=")?.trim();
    if spec.contains(',') {
        return None;
    }
    let (start_raw, end_raw) = spec.split_once('-')?;
    let start_raw = start_raw.trim();
    let end_raw = end_raw.trim();

    if start_raw.is_empty() {
        // A suffix range: the last N bytes.
        let suffix: u64 = end_raw.parse().ok()?;
        if suffix == 0 || total == 0 {
            return None;
        }
        let start = total.saturating_sub(suffix);
        return Some((start, total - 1));
    }

    let start: u64 = start_raw.parse().ok()?;
    if start >= total {
        return None;
    }
    let end = if end_raw.is_empty() {
        total - 1
    } else {
        end_raw.parse::<u64>().ok()?.min(total - 1)
    };
    if end < start {
        return None;
    }
    Some((start, end))
}

/// Lower-case hex of a digest.
fn hex_lower(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Insert a header, ignoring a value that cannot be rendered.
fn insert_header(response: &mut Response, name: header::HeaderName, value: &str) {
    if let Ok(value) = header::HeaderValue::from_str(value) {
        response.headers_mut().insert(name, value);
    }
}

/// The metadata shape the client API answers with for an attachment.
pub fn meta_of(row: &ferroma_storage::models::AttachmentRow) -> AttachmentMetaResponse {
    AttachmentMetaResponse {
        id: row.id,
        filename: row.filename.clone(),
        content_type: row.content_type.clone(),
        size_bytes: row.size_bytes,
        sha256: row.checksum_sha256.clone(),
        is_inline: row.is_inline,
        content_id: row.content_id.clone(),
    }
}

/// The list shape used where an attachment appears inside a message.
pub fn summary_of(row: &ferroma_storage::models::AttachmentRow) -> AttachmentResponse {
    AttachmentResponse::from_row(row)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_parsing_covers_the_documented_cases() {
        assert_eq!(parse_range("bytes=0-99", 1000), Some((0, 99)));
        assert_eq!(parse_range("bytes=500-", 1000), Some((500, 999)));
        assert_eq!(parse_range("bytes=-100", 1000), Some((900, 999)));
        // A range that runs past the end is clamped, not refused.
        assert_eq!(parse_range("bytes=900-5000", 1000), Some((900, 999)));
        // Past the end entirely: serve the whole body instead.
        assert_eq!(parse_range("bytes=1000-1200", 1000), None);
        // Multi-range is not honoured here.
        assert_eq!(parse_range("bytes=0-1,4-5", 1000), None);
        // Malformed.
        assert_eq!(parse_range("items=0-1", 1000), None);
        assert_eq!(parse_range("bytes=abc-def", 1000), None);
        assert_eq!(parse_range("bytes=500-100", 1000), None);
        assert_eq!(parse_range("bytes=-0", 1000), None);
        assert_eq!(parse_range("bytes=0-0", 0), None);
    }

    #[test]
    fn file_names_cannot_carry_a_path() {
        assert_eq!(sanitize_filename("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_filename("C:\\Users\\evil.txt"), "evil.txt");
        assert_eq!(sanitize_filename("  report.pdf  "), "report.pdf");
        assert_eq!(sanitize_filename("quote\"name.txt"), "quote'name.txt");
        assert_eq!(sanitize_filename("new\nline.txt"), "new line.txt");
        assert_eq!(sanitize_filename(""), "");
        assert_eq!(sanitize_filename("."), "");
    }

    #[test]
    fn a_very_long_file_name_is_truncated() {
        let long = "a".repeat(500);
        assert_eq!(sanitize_filename(&long).chars().count(), 200);
    }

    #[test]
    fn content_disposition_is_always_quoted_and_never_empty() {
        assert_eq!(
            content_disposition(Some("report.pdf")),
            "attachment; filename=\"report.pdf\""
        );
        assert_eq!(
            content_disposition(Some("../../x")),
            "attachment; filename=\"x\""
        );
        assert_eq!(content_disposition(None), "attachment");
        assert_eq!(content_disposition(Some("   ")), "attachment");
    }

    #[test]
    fn upload_tokens_are_unique_and_prefixed() {
        let a = new_upload_token();
        let b = new_upload_token();
        assert_ne!(a, b);
        assert!(a.starts_with("up_"));
        assert_eq!(a.len(), 3 + 32);
    }

    #[test]
    fn the_upload_status_shape_serialises_as_documented() {
        let json = serde_json::to_value(UploadStatusResponse {
            attachment_id: 991,
            size_bytes: 4_194_304,
            chunk_size: 1_048_576,
            chunk_count: 4,
            received: vec![0, 1, 3],
            complete: false,
        })
        .expect("must serialise");
        assert_eq!(json["attachment_id"], 991);
        assert_eq!(json["size_bytes"], 4_194_304);
        assert_eq!(json["chunk_size"], 1_048_576);
        assert_eq!(json["chunk_count"], 4);
        assert_eq!(json["received"], serde_json::json!([0, 1, 3]));
        assert_eq!(json["complete"], false);
        // The field names are the ones `docs/fcp.md` §6 freezes for the client.
        assert!(json.get("total_chunks").is_none(), "{json}");
        assert!(json.get("missing").is_none(), "{json}");
    }

    #[test]
    fn the_upload_init_shape_carries_the_token() {
        let json = serde_json::to_value(UploadInitResponse {
            attachment_id: 12,
            chunk_size: 1_048_576,
            upload_token: "up_abc".into(),
        })
        .expect("must serialise");
        assert_eq!(json["attachment_id"], 12);
        assert_eq!(json["upload_token"], "up_abc");
    }

    #[test]
    fn the_meta_shape_carries_the_digest_for_etag() {
        let row = ferroma_storage::models::AttachmentRow {
            id: 991,
            message_id: 4821,
            filename: Some("invoice.pdf".into()),
            content_type: "application/pdf".into(),
            size_bytes: 24831,
            storage_path: "ab/cd/x".into(),
            content_id: None,
            is_inline: false,
            checksum_sha256: Some("deadbeef".into()),
            created_at: chrono::Utc::now(),
        };
        let meta = meta_of(&row);
        assert_eq!(meta.sha256.as_deref(), Some("deadbeef"));
        let json = serde_json::to_value(&meta).expect("must serialise");
        assert_eq!(json["id"], 991);
        assert_eq!(json["size_bytes"], 24831);
        // The blob's internal path is never published.
        assert!(json.get("storage_path").is_none(), "{json}");

        let summary = summary_of(&row);
        assert_eq!(summary.filename.as_deref(), Some("invoice.pdf"));
    }

    #[test]
    fn init_requires_a_real_size() {
        let request: UploadInitRequest =
            serde_json::from_value(serde_json::json!({ "filename": "a.bin", "size_bytes": 10 }))
                .expect("must parse");
        assert_eq!(request.size_bytes, 10);
        assert!(request.content_type.is_none());
        let bad = serde_json::from_value::<UploadInitRequest>(
            serde_json::json!({ "filename": "a.bin" }),
        );
        assert!(bad.is_err(), "size_bytes is required");
    }

    #[test]
    fn complete_requires_a_digest() {
        assert!(serde_json::from_value::<CompleteUploadRequest>(serde_json::json!({})).is_err());
        let request: CompleteUploadRequest =
            serde_json::from_value(serde_json::json!({ "sha256": "abc" })).expect("must parse");
        assert_eq!(request.sha256, "abc");
    }

    #[test]
    fn chunk_query_requires_an_index() {
        assert!(serde_json::from_value::<ChunkQuery>(serde_json::json!({})).is_err());
        let query: ChunkQuery =
            serde_json::from_value(serde_json::json!({ "index": 7 })).expect("must parse");
        assert_eq!(query.index, 7);
    }
}
