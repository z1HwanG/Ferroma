//! `docs/api.md` §5.2 — messages.
//!
//! # The single-message shape
//!
//! `GET /messages/:id` has to hand Webmail everything a *reply* needs, which is why
//! the response carries the RFC 5322 threading headers
//! ([`ThreadingHeaders`](crate::routes::mail::store::ThreadingHeaders)) and not just
//! the stored row: `message_id_header`, the message's own `in_reply_to` and its
//! `references`, plus the `cc` and `reply_to` lists. Those live in the raw bytes, so
//! the handler reads the Maildir file and parses it once for the whole response.
//!
//! # Sending
//!
//! `POST /messages` writes the copy into the sender's `Sent` folder, queues one
//! `mail_queue` row per recipient through the delivery seam, publishes `mail.sent`
//! and answers `{message_id, queued, recipients}`. With `draft: true` it files the
//! message in `Drafts` with `\Draft` instead and queues nothing — a draft needs no
//! recipient at all.

use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use ferroma_core::{FerromaError, MailboxId, MessageId};
use ferroma_storage::repository::MessageSearch;
use serde::{Deserialize, Serialize};

use crate::error::ApiError;
use crate::extract::{AuthUser, IdempotencyKey, Pagination, Page};
use crate::routes::mail::ownership::{owned_folder, owned_live_message, owned_mailbox};
use crate::routes::mail::shapes::{
    AddressResponse, AttachmentResponse, MessageDetailResponse, MessageSummaryResponse, SendResponse,
};
use crate::routes::mail::store;
use crate::service::SendRequest;
use crate::state::AppState;

/// The filters `GET /api/v1/messages` accepts.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct MessageListQuery {
    /// Restrict to one address.
    pub mailbox_id: Option<i64>,
    /// Restrict to one folder.
    pub folder_id: Option<i64>,
    /// Free-text search over subject, sender and snippet.
    pub query: Option<String>,
    /// Only messages without `\Seen`.
    pub unread: Option<bool>,
    /// Only messages with `\Flagged`.
    pub flagged: Option<bool>,
    /// Only messages that carry attachments.
    pub has_attachments: Option<bool>,
    /// Only messages at or after this instant.
    pub since: Option<String>,
    /// Only messages before this instant.
    pub before: Option<String>,
    /// Page size.
    pub limit: Option<i64>,
    /// Rows to skip.
    pub offset: Option<i64>,
}

/// The `POST /api/v1/messages` body.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SendMessageRequest {
    /// The `From` address.
    pub from: String,
    /// Display name for `From`.
    pub from_name: Option<String>,
    /// `To` recipients.
    #[serde(default)]
    pub to: Vec<String>,
    /// `Cc` recipients.
    #[serde(default)]
    pub cc: Vec<String>,
    /// `Bcc` recipients.
    #[serde(default)]
    pub bcc: Vec<String>,
    /// Subject.
    #[serde(default)]
    pub subject: String,
    /// Plain-text body.
    pub text: Option<String>,
    /// HTML body.
    pub html: Option<String>,
    /// Attachment ids.
    #[serde(default)]
    pub attachments: Vec<i64>,
    /// The message being replied to.
    pub in_reply_to: Option<String>,
    /// The reference chain.
    #[serde(default)]
    pub references: Vec<String>,
    /// A `Reply-To` header.
    pub reply_to: Option<String>,
    /// A draft id this send completes, when the client kept one.
    pub draft_id: Option<i64>,
    /// File the message in `Drafts` instead of sending it.
    #[serde(default)]
    pub draft: bool,
}

impl SendMessageRequest {
    /// Convert into the service's own request type.
    pub fn into_send_request(self) -> SendRequest {
        SendRequest {
            from: self.from,
            from_name: self.from_name,
            to: self.to,
            cc: self.cc,
            bcc: self.bcc,
            subject: self.subject,
            text: self.text,
            html: self.html,
            attachment_ids: self.attachments,
            in_reply_to: self.in_reply_to,
            references: self.references,
            reply_to: self.reply_to,
            draft: self.draft,
        }
    }
}

/// The `PATCH /api/v1/messages/:id` body.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PatchMessageRequest {
    /// `\Seen`.
    pub seen: Option<bool>,
    /// `\Flagged`.
    pub flagged: Option<bool>,
    /// `\Answered`.
    pub answered: Option<bool>,
    /// `\Deleted`.
    pub deleted: Option<bool>,
}

/// The `{folder_id}` body shared by move and copy.
#[derive(Debug, Clone, Deserialize)]
pub struct FolderTargetRequest {
    /// The destination folder.
    pub folder_id: i64,
}

/// The `POST /api/v1/messages/batch` body.
#[derive(Debug, Clone, Deserialize)]
pub struct BatchRequest {
    /// `read`, `unread`, `flag`, `unflag`, `move` or `delete`.
    pub operation: String,
    /// The messages to act on.
    pub ids: Vec<i64>,
    /// The destination, for `move`.
    pub folder_id: Option<i64>,
}

/// The `POST /api/v1/messages/batch` response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchResponse {
    /// The operation that ran.
    pub operation: String,
    /// How many messages it touched.
    pub affected: usize,
    /// The ids it touched.
    pub ids: Vec<i64>,
}

/// `GET /api/v1/messages`
pub async fn list_messages(
    State(state): State<AppState>,
    user: AuthUser,
    Query(query): Query<MessageListQuery>,
) -> Result<Json<Page<MessageSummaryResponse>>, ApiError> {
    let pagination = Pagination::clamped(query.limit, query.offset);

    // Scope the search to the caller: a folder implies its address, and a bare
    // `mailbox_id` must be one of the caller's.
    let (mailbox_scope, folder_scope) = resolve_scope(&state, &user, &query).await?;

    let search = MessageSearch {
        folder_id: folder_scope,
        mailbox_id: mailbox_scope,
        subject: None,
        sender: None,
        text: query.query.clone().filter(|q| !q.trim().is_empty()),
        unread_only: query.unread.unwrap_or(false),
        flagged_only: query.flagged.unwrap_or(false),
        with_attachments_only: query.has_attachments.unwrap_or(false),
        since: query.since.as_deref().and_then(parse_instant),
        before: query.before.as_deref().and_then(parse_instant),
        limit: pagination.limit,
        offset: pagination.offset,
    };

    let rows = state.repos.messages.search(search.clone()).await?;
    let total = count_search(&state, &search).await;

    let mut items = Vec::with_capacity(rows.len());
    for row in rows {
        items.push(summary_response(&state, &row).await?);
    }

    Ok(Json(pagination.page(items, total)))
}

/// The total number of rows a search matches, for the `total` field.
///
/// `ferroma-storage`'s `MessageSearch` pages a result but does not count it, and this
/// crate must not edit that crate. The count therefore walks the pages the search
/// returns — bounded, because the walk stops as soon as a short page arrives — rather
/// than issuing SQL of its own, which would need a direct `sqlx` dependency this crate
/// deliberately does not have.
async fn count_search(state: &AppState, search: &MessageSearch) -> i64 {
    const PAGE: i64 = 500;
    let mut total = 0i64;
    let mut offset = 0i64;
    loop {
        let mut page = search.clone();
        page.limit = PAGE;
        page.offset = offset;
        let Ok(rows) = state.repos.messages.search(page).await else {
            break;
        };
        total += rows.len() as i64;
        if (rows.len() as i64) < PAGE {
            break;
        }
        offset += PAGE;
        // A safety valve so a pathological query cannot spin forever.
        if offset > 100_000 {
            break;
        }
    }
    total
}

/// Work out which address and folder the caller is allowed to search.
async fn resolve_scope(
    state: &AppState,
    user: &AuthUser,
    query: &MessageListQuery,
) -> Result<(Option<MailboxId>, Option<MailboxId>), ApiError> {
    let folder_scope = match query.folder_id {
        Some(id) => {
            let (folder, _mailbox) = owned_folder(&state.repos, MailboxId::new(id), user.user_id())
                .await?;
            Some(folder.folder_id())
        }
        None => None,
    };

    let mailbox_scope = match query.mailbox_id {
        Some(id) => Some(owned_mailbox(&state.repos, MailboxId::new(id), user.user_id()).await?.mailbox_id()),
        None => match folder_scope {
            Some(_) => None,
            None => {
                // No scope at all: search every address the caller owns, by restricting
                // to the primary one when there is exactly one.
                state
                    .repos
                    .mailboxes
                    .find_primary(user.user_id())
                    .await?
                    .map(|mailbox| mailbox.mailbox_id())
            }
        },
    };

    Ok((mailbox_scope, folder_scope))
}

/// Build the list-view shape, including the `To` list a client needs.
pub async fn summary_response(
    state: &AppState,
    row: &ferroma_storage::models::Message,
) -> Result<MessageSummaryResponse, ApiError> {
    let recipients = state
        .repos
        .messages
        .recipients(MessageId::new(row.id))
        .await
        .unwrap_or_default();
    Ok(MessageSummaryResponse {
        id: row.id,
        uid: row.uid,
        folder_id: row.folder_id,
        mailbox_id: row.mailbox_id,
        subject: row.subject.clone(),
        from: row
            .sender
            .as_ref()
            .map(|address| AddressResponse {
                address: address.clone(),
                name: row.sender_name.clone(),
            }),
        to: by_kind(&recipients, "to"),
        cc: by_kind(&recipients, "cc"),
        reply_to: by_kind(&recipients, "reply-to"),
        snippet: row.snippet.clone(),
        flags: row.flags.clone(),
        size_bytes: row.size_bytes,
        has_attachments: row.has_attachments,
        attachment_count: row.attachment_count,
        is_draft: row.is_draft,
        internal_date: row.internal_date,
        sent_at: row.sent_at,
        rfc_message_id: row.rfc_message_id.clone(),
    })
}

/// The recipients of one kind, in order.
pub fn by_kind(
    recipients: &[ferroma_storage::models::MessageRecipient],
    kind: &str,
) -> Vec<AddressResponse> {
    recipients
        .iter()
        .filter(|row| row.kind == kind)
        .map(AddressResponse::from_recipient)
        .collect()
}

/// `GET /api/v1/messages/:id`
pub async fn get_message(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
) -> Result<Json<MessageDetailResponse>, ApiError> {
    let (message, _mailbox, bytes) = state
        .mail_service
        .raw_message(MessageId::new(id), user.user_id())
        .await?;
    let detail = detail_response(&state, &message, &bytes).await?;
    Ok(Json(detail))
}

/// Assemble the full single-message shape from the row and its bytes.
pub async fn detail_response(
    state: &AppState,
    row: &ferroma_storage::models::Message,
    raw: &[u8],
) -> Result<MessageDetailResponse, ApiError> {
    let parsed = ferroma_mail::ParsedMessage::parse(raw).ok();
    let recipients = state
        .repos
        .messages
        .recipients(MessageId::new(row.id))
        .await
        .unwrap_or_default();
    let attachments = state
        .repos
        .attachments
        .list_by_message(MessageId::new(row.id))
        .await
        .unwrap_or_default();

    let threading = store::threading_headers(raw);
    let text_body = parsed.as_ref().and_then(|message| message.text_body());
    let html_body = parsed
        .as_ref()
        .and_then(|message| message.html_body())
        .map(|html| store::sanitize_html(&html));

    Ok(MessageDetailResponse {
        id: row.id,
        uid: row.uid,
        folder_id: row.folder_id,
        mailbox_id: row.mailbox_id,
        subject: row.subject.clone(),
        from: row.sender.as_ref().map(|address| AddressResponse {
            address: address.clone(),
            name: row.sender_name.clone(),
        }),
        to: by_kind(&recipients, "to"),
        cc: by_kind(&recipients, "cc"),
        reply_to: by_kind(&recipients, "reply-to"),
        flags: row.flags.clone(),
        size_bytes: row.size_bytes,
        snippet: row.snippet.clone(),
        text_body,
        html_body,
        message_id_header: threading.message_id.or_else(|| row.rfc_message_id.clone()),
        in_reply_to: threading.in_reply_to,
        references: threading.references,
        internal_date: row.internal_date,
        sent_at: row.sent_at,
        is_draft: row.is_draft,
        has_attachments: row.has_attachments,
        attachment_count: row.attachment_count,
        attachments: attachments.iter().map(AttachmentResponse::from_row).collect(),
    })
}

/// `GET /api/v1/messages/:id/raw` — the RFC 5322 bytes, `message/rfc822`.
pub async fn get_raw_message(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
) -> Result<Response, ApiError> {
    let (message, _mailbox, bytes) = state
        .mail_service
        .raw_message(MessageId::new(id), user.user_id())
        .await?;

    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("message/rfc822"),
    );
    let filename = format!("message-{}.eml", message.id);
    if let Ok(value) = header::HeaderValue::from_str(&format!(
        "attachment; filename=\"{filename}\""
    )) {
        headers.insert(header::CONTENT_DISPOSITION, value);
    }
    if let Some(etag) = message.checksum_sha256.as_deref() {
        if let Ok(value) = header::HeaderValue::from_str(&format!("\"{etag}\"")) {
            headers.insert(header::ETAG, value);
        }
    }

    Ok((StatusCode::OK, headers, bytes).into_response())
}

/// `POST /api/v1/messages` — send, or save a draft.
///
/// An `Idempotency-Key` header makes the send safe to retry: a repeat replays the
/// recorded response instead of sending twice (`docs/api.md` §1.5).
pub async fn send_message(
    State(state): State<AppState>,
    user: AuthUser,
    key: IdempotencyKey,
    Json(request): Json<SendMessageRequest>,
) -> Result<(StatusCode, Json<SendResponse>), ApiError> {
    let draft = request.draft;
    let send = request.into_send_request();
    let user_id = user.user_id();

    let result = idempotent(
        &state,
        key.as_deref(),
        user_id,
        if draft { "save_draft" } else { "send_message" },
        || {
            let state = state.clone();
            let send = send.clone();
            async move { state.mail_service.send(user_id, &send).await }
        },
    )
    .await?;

    Ok((
        if draft {
            StatusCode::CREATED
        } else {
            StatusCode::OK
        },
        Json(SendResponse {
            message_id: result.message_id,
            queued: result.queued,
            recipients: result.recipients,
        }),
    ))
}

/// `PATCH /api/v1/messages/:id`
pub async fn patch_message(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
    Json(request): Json<PatchMessageRequest>,
) -> Result<Json<MessageDetailResponse>, ApiError> {
    let updated = state
        .mail_service
        .set_message_flags(
            MessageId::new(id),
            user.user_id(),
            request.seen,
            request.flagged,
            request.answered,
            request.deleted,
        )
        .await?;
    let raw = state
        .mail_service
        .maildir()
        .read(&updated.storage_path)
        .unwrap_or_default();
    Ok(Json(detail_response(&state, &updated, &raw).await?))
}

/// `POST /api/v1/messages/:id/move`
pub async fn move_message(
    State(state): State<AppState>,
    user: AuthUser,
    key: IdempotencyKey,
    Path(id): Path<i64>,
    Json(request): Json<FolderTargetRequest>,
) -> Result<Json<MessageSummaryResponse>, ApiError> {
    let user_id = user.user_id();
    let moved = idempotent(&state, key.as_deref(), user_id, "move_message", || {
        let state = state.clone();
        async move {
            state
                .mail_service
                .move_message(MessageId::new(id), user_id, MailboxId::new(request.folder_id))
                .await
        }
    })
    .await?;
    Ok(Json(summary_response(&state, &moved).await?))
}

/// `POST /api/v1/messages/:id/copy`
pub async fn copy_message(
    State(state): State<AppState>,
    user: AuthUser,
    key: IdempotencyKey,
    Path(id): Path<i64>,
    Json(request): Json<FolderTargetRequest>,
) -> Result<(StatusCode, Json<MessageSummaryResponse>), ApiError> {
    let user_id = user.user_id();
    let copied = idempotent(&state, key.as_deref(), user_id, "copy_message", || {
        let state = state.clone();
        async move {
            state
                .mail_service
                .copy_message(MessageId::new(id), user_id, MailboxId::new(request.folder_id))
                .await
        }
    })
    .await?;
    Ok((
        StatusCode::CREATED,
        Json(summary_response(&state, &copied).await?),
    ))
}

/// The `?permanent=true` flag of `DELETE /api/v1/messages/:id`, plus the idempotency
/// key a retrying client sends.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct DeleteMessageQuery {
    /// Remove the row instead of moving the message to `Trash`.
    pub permanent: Option<bool>,
    /// An idempotency key, as `docs/api.md` §1.5 documents for the Client API.
    pub operation_id: Option<String>,
}

/// `DELETE /api/v1/messages/:id`
///
/// A retried delete is safe: the second call finds nothing and answers `404`, which the
/// client's Outbox reads as success — the message is gone, which was the point.
pub async fn delete_message(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
    Query(query): Query<DeleteMessageQuery>,
) -> Result<StatusCode, ApiError> {
    let permanent = query.permanent.unwrap_or(false);
    let user_id = user.user_id();
    idempotent(
        &state,
        query.operation_id.as_deref(),
        user_id,
        "delete_message",
        || {
            let state = state.clone();
            async move {
                state
                    .mail_service
                    .delete_message(MessageId::new(id), user_id, permanent)
                    .await
            }
        },
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /api/v1/messages/batch`
pub async fn batch_messages(
    State(state): State<AppState>,
    user: AuthUser,
    Json(request): Json<BatchRequest>,
) -> Result<Json<BatchResponse>, ApiError> {
    if request.ids.is_empty() {
        return Err(ApiError::new(FerromaError::Invalid(
            "ids must not be empty".to_string(),
        )));
    }
    if request.ids.len() > 1000 {
        return Err(ApiError::new(FerromaError::LimitExceeded(
            "at most 1000 messages may be changed in one batch".to_string(),
        )));
    }

    let operation = request.operation.trim().to_ascii_lowercase();
    let mut affected = Vec::with_capacity(request.ids.len());

    for raw_id in &request.ids {
        let id = MessageId::new(*raw_id);
        let outcome = match operation.as_str() {
            "read" => {
                state
                    .mail_service
                    .set_message_flags(id, user.user_id(), Some(true), None, None, None)
                    .await
                    .map(|_| ())
            }
            "unread" => {
                state
                    .mail_service
                    .set_message_flags(id, user.user_id(), Some(false), None, None, None)
                    .await
                    .map(|_| ())
            }
            "flag" => {
                state
                    .mail_service
                    .set_message_flags(id, user.user_id(), None, Some(true), None, None)
                    .await
                    .map(|_| ())
            }
            "unflag" => {
                state
                    .mail_service
                    .set_message_flags(id, user.user_id(), None, Some(false), None, None)
                    .await
                    .map(|_| ())
            }
            "move" => {
                let Some(folder_id) = request.folder_id else {
                    return Err(ApiError::new(FerromaError::Invalid(
                        "operation `move` needs a folder_id".to_string(),
                    )));
                };
                state
                    .mail_service
                    .move_message(id, user.user_id(), MailboxId::new(folder_id))
                    .await
                    .map(|_| ())
            }
            "delete" => {
                state
                    .mail_service
                    .delete_message(id, user.user_id(), false)
                    .await
            }
            other => {
                return Err(ApiError::new(FerromaError::Invalid(format!(
                    "unknown batch operation {other:?}"
                ))));
            }
        };

        match outcome {
            Ok(()) => affected.push(*raw_id),
            // A foreign or missing id is not confirmed: it is simply skipped, which is
            // what a batch API should do rather than failing the whole request.
            Err(err) if err.code() == "not_found" => {}
            Err(err) => return Err(ApiError::from(err)),
        }
    }

    Ok(Json(BatchResponse {
        operation,
        affected: affected.len(),
        ids: affected,
    }))
}

/// Whether a message row currently carries a flag.
///
/// The `messages.flags` column is a space-separated, lower-case flag string (that is
/// what the repository writes), while `ferroma_mail::Flags` parses the comma-separated
/// spelling. Splitting on both separators keeps the two vocabularies agreeing.
pub fn has_flag(flags: &str, name: &str) -> bool {
    flags
        .split([' ', ','])
        .filter(|token| !token.is_empty())
        .any(|token| token.eq_ignore_ascii_case(name))
}

/// Parse an RFC 3339 instant from a query parameter.
///
/// A bare date (`2026-01-01`) is accepted as midnight UTC, because that is what a client
/// typing `after:2026-01-01` means and `docs/fcp.md` §10 uses exactly that form.
pub fn parse_instant(raw: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    let raw = raw.trim();
    if let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(raw) {
        return Some(parsed.with_timezone(&chrono::Utc));
    }
    if let Ok(date) = chrono::NaiveDate::parse_from_str(raw, "%Y-%m-%d") {
        return date.and_hms_opt(0, 0, 0).map(|naive| naive.and_utc());
    }
    None
}

/// `GET /api/v1/messages/:id` with the raw bytes included, for the client API's
/// `…/raw` route which reads them through the same ownership check.
pub async fn raw_bytes_for(
    state: &AppState,
    id: MessageId,
    user: ferroma_core::UserId,
) -> Result<Vec<u8>, ApiError> {
    let (_message, _mailbox, bytes) = state.mail_service.raw_message(id, user).await?;
    Ok(bytes)
}

/// A convenience used by tests and by the client routes: the owned message row plus
/// its parsed recipients.
pub async fn recipients_of(
    state: &AppState,
    id: MessageId,
) -> Result<Vec<ferroma_storage::models::MessageRecipient>, ApiError> {
    Ok(state.repos.messages.recipients(id).await?)
}

/// Read a message the caller owns, without its bytes.
pub async fn owned_by(
    state: &AppState,
    id: MessageId,
    user: ferroma_core::UserId,
) -> Result<ferroma_storage::models::Message, ApiError> {
    let (message, _mailbox) = owned_live_message(&state.repos, id, user).await?;
    Ok(message)
}

/// Run `operation` under an idempotency key, or straight through when there is none.
///
/// `docs/api.md` §1.5: a key already **completed** replays its recorded response
/// verbatim; a key whose original request is still in flight (or whose process died
/// mid-request) answers `409 conflict` with "… has not finished; retry later". That is
/// [`ferroma_sync::SyncService::with_operation`]'s contract, so this is a thin adapter
/// that also applies the `ApiError` envelope.
pub async fn idempotent<T, F, Fut>(
    state: &AppState,
    key: Option<&str>,
    user: ferroma_core::UserId,
    kind: &str,
    operation: F,
) -> Result<T, ApiError>
where
    T: serde::Serialize + serde::de::DeserializeOwned,
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<T, FerromaError>>,
{
    match key.map(str::trim).filter(|key| !key.is_empty()) {
        Some(key) => state
            .sync
            .with_operation(key, user, kind, operation)
            .await
            .map_err(ApiError::from),
        None => operation().await.map_err(ApiError::from),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferroma_storage::models::MessageRecipient;

    fn recipient(kind: &str, address: &str) -> MessageRecipient {
        MessageRecipient {
            id: 1,
            message_id: 1,
            kind: kind.into(),
            address: address.into(),
            display_name: None,
            ordinal: 0,
        }
    }

    #[test]
    fn by_kind_filters_and_preserves_order() {
        let rows = vec![
            recipient("to", "a@b.c"),
            recipient("cc", "d@e.f"),
            recipient("to", "g@h.i"),
        ];
        let to = by_kind(&rows, "to");
        assert_eq!(to.len(), 2);
        assert_eq!(to[0].address, "a@b.c");
        assert_eq!(to[1].address, "g@h.i");
        assert_eq!(by_kind(&rows, "cc").len(), 1);
        assert!(by_kind(&rows, "bcc").is_empty());
    }

    #[test]
    fn instant_parsing_accepts_rfc3339_and_rejects_junk() {
        let parsed = parse_instant("2026-09-16T12:00:00Z").expect("valid instant");
        assert_eq!(
            parsed.timestamp(),
            chrono::DateTime::parse_from_rfc3339("2026-09-16T12:00:00Z")
                .expect("valid instant")
                .timestamp()
        );
        assert!(parse_instant("yesterday").is_none());
        assert!(parse_instant("").is_none());
        // An offset is normalised to UTC.
        let offset = parse_instant("2026-09-16T14:00:00+02:00").expect("valid instant");
        assert_eq!(offset, parsed);
    }

    #[test]
    fn flag_detection_reads_the_canonical_string() {
        assert!(has_flag("seen flagged", "seen"));
        assert!(has_flag("seen,flagged", "seen"));
        assert!(has_flag("SEEN", "seen"));
        assert!(has_flag(" seen  flagged ", "flagged"));
        assert!(!has_flag("", "seen"));
        assert!(!has_flag("flagged", "seen"));
        // A flag whose name merely contains another is not a match.
        assert!(!has_flag("unseen", "seen"));
    }

    #[test]
    fn send_request_maps_onto_the_service_type() {
        let request = SendMessageRequest {
            from: "alice@example.com".into(),
            to: vec!["bob@example.net".into()],
            subject: "hi".into(),
            text: Some("body".into()),
            attachments: vec![7],
            draft: false,
            ..SendMessageRequest::default()
        };
        let send = request.into_send_request();
        assert_eq!(send.from, "alice@example.com");
        assert_eq!(send.attachment_ids, vec![7]);
        assert!(!send.draft);
    }

    #[test]
    fn a_draft_request_carries_no_recipients_and_is_flagged() {
        let request = SendMessageRequest {
            from: "alice@example.com".into(),
            draft: true,
            subject: "later".into(),
            ..SendMessageRequest::default()
        };
        let send = request.into_send_request();
        assert!(send.draft);
        assert!(send.to.is_empty());
    }

    #[test]
    fn batch_response_serialises_the_documented_fields() {
        let json = serde_json::to_value(BatchResponse {
            operation: "read".into(),
            affected: 2,
            ids: vec![1, 2],
        })
        .expect("must serialise");
        assert_eq!(json["operation"], "read");
        assert_eq!(json["affected"], 2);
        assert_eq!(json["ids"], serde_json::json!([1, 2]));
    }

    #[test]
    fn message_list_query_defaults_are_empty() {
        let query = MessageListQuery::default();
        assert!(query.mailbox_id.is_none());
        assert!(query.unread.is_none());
        assert!(query.limit.is_none());
    }

    #[test]
    fn patch_request_accepts_every_documented_flag() {
        let request: PatchMessageRequest = serde_json::from_value(serde_json::json!({
            "seen": true,
            "flagged": false,
            "answered": true,
            "deleted": false
        }))
        .expect("must deserialise");
        assert_eq!(request.seen, Some(true));
        assert_eq!(request.flagged, Some(false));
        assert_eq!(request.answered, Some(true));
        assert_eq!(request.deleted, Some(false));

        let empty: PatchMessageRequest =
            serde_json::from_value(serde_json::json!({})).expect("must deserialise");
        assert!(empty.seen.is_none());
    }

    #[test]
    fn batch_request_deserialises_with_and_without_a_folder() {
        let request: BatchRequest = serde_json::from_value(serde_json::json!({
            "operation": "move",
            "ids": [1, 2],
            "folder_id": 5
        }))
        .expect("must deserialise");
        assert_eq!(request.operation, "move");
        assert_eq!(request.folder_id, Some(5));

        let plain: BatchRequest = serde_json::from_value(serde_json::json!({
            "operation": "read",
            "ids": [3]
        }))
        .expect("must deserialise");
        assert!(plain.folder_id.is_none());
    }

    #[test]
    fn send_request_deserialises_the_documented_body() {
        let request: SendMessageRequest = serde_json::from_value(serde_json::json!({
            "from": "alice@example.com",
            "to": ["bob@example.net"],
            "cc": ["carol@example.org"],
            "subject": "Invoice",
            "text": "hi",
            "html": "<p>hi</p>",
            "attachments": [1, 2],
            "in_reply_to": "<a@b.c>",
            "references": ["<x@y.z>"]
        }))
        .expect("must deserialise");
        assert_eq!(request.to.len(), 1);
        assert_eq!(request.cc.len(), 1);
        assert_eq!(request.attachments, vec![1, 2]);
        assert_eq!(request.references, vec!["<x@y.z>"]);
        assert!(!request.draft);
    }

    #[test]
    fn folder_target_request_requires_a_folder() {
        assert!(serde_json::from_value::<FolderTargetRequest>(serde_json::json!({})).is_err());
        let target: FolderTargetRequest =
            serde_json::from_value(serde_json::json!({ "folder_id": 9 })).expect("must parse");
        assert_eq!(target.folder_id, 9);
    }

    #[test]
    fn delete_query_defaults_to_soft_delete() {
        let query: DeleteMessageQuery =
            serde_json::from_value(serde_json::json!({})).expect("must parse");
        assert_eq!(query.permanent, None);
        assert!(!query.permanent.unwrap_or(false));
        assert!(query.operation_id.is_none());

        let query: DeleteMessageQuery = serde_json::from_value(serde_json::json!({
            "permanent": true,
            "operation_id": "op_del_once"
        }))
        .expect("must parse");
        assert!(query.permanent.unwrap_or(false));
        assert_eq!(query.operation_id.as_deref(), Some("op_del_once"));
    }
}
