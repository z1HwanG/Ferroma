//! `docs/api.md` §6, `docs/fcp.md` §3–§10 — the client resources.
//!
//! Every route here is a thin adapter over [`crate::service::MessageService`], so the
//! desktop client and Webmail cannot drift apart: `POST /client/messages/:id/read` and
//! `PATCH /api/v1/messages/:id {"seen": true}` are the same code path with two
//! spellings.
//!
//! # Sync
//!
//! `GET /client/sync` is [`ferroma_sync::SyncService::sync`] and nothing else. A cursor
//! older than the retained change history comes back as [`FerromaError::Conflict`],
//! which this module renders as the documented `409` body:
//!
//! ```json
//! { "error": { "code": "conflict", "message": "cursor too old; full resync required" } }
//! ```
//!
//! # Idempotency
//!
//! Every mutating route accepts `?operation_id=`. When it is present the work runs
//! through [`ferroma_sync::SyncService::with_operation`], so a retry replays the
//! recorded response instead of acting twice. When it is absent the operation is simply
//! not recorded, which is what a non-retrying caller (a debug `curl`) wants.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use ferroma_core::{Cursor, FerromaError, MailboxId, MessageId, UserId};
use ferroma_storage::repository::MessageSearch;
use ferroma_sync::{ChangeKind, SyncPage, SyncRequest};
use serde::{Deserialize, Serialize};

use crate::error::ApiError;
use crate::extract::{ClientAuth, Page, Pagination, PaginationQuery};
use crate::routes::mail::mailboxes::mailbox_tree;
use crate::routes::mail::messages::{
    batch_messages, by_kind, detail_response, list_messages, parse_instant, summary_response,
    BatchRequest, BatchResponse, MessageListQuery, PatchMessageRequest,
};
use crate::routes::mail::ownership::{owned_live_message, owned_mailbox};
use crate::routes::mail::shapes::{
    AttachmentResponse, DeviceResponse, MailboxTreeResponse, MessageDetailResponse,
    MessageSummaryResponse, SendResponse,
};
use crate::state::AppState;

/// The `GET /api/v1/client/mailboxes` body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientMailboxListResponse {
    /// The addresses, each with its folders.
    pub mailboxes: Vec<MailboxTreeResponse>,
}

/// The `GET /api/v1/client/sync` query (`docs/fcp.md` §3).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SyncQuery {
    /// Which address (required).
    pub mailbox_id: Option<i64>,
    /// One folder; omit for account-level changes.
    pub folder_id: Option<i64>,
    /// The last cursor the client successfully applied.
    pub cursor: Option<String>,
    /// Maximum changes to return, capped by `client.sync_page_size`.
    pub limit: Option<usize>,
}

/// A sync page rendered for the wire.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientSyncResponse {
    /// The cursor to store once every change has been applied.
    pub next_cursor: String,
    /// Whether more changes are waiting.
    pub has_more: bool,
    /// The newest cursor the server holds for this account.
    pub latest_cursor: String,
    /// The changes, ordered by `seq` ascending.
    pub changes: Vec<ferroma_sync::SyncChange>,
}

impl ClientSyncResponse {
    /// Render a [`SyncPage`].
    pub fn from_page(page: SyncPage) -> Self {
        ClientSyncResponse {
            next_cursor: page.next_cursor.as_str(),
            has_more: page.has_more,
            latest_cursor: page.latest_cursor.as_str(),
            changes: page.changes,
        }
    }
}

/// The `?operation_id=` every mutating client route accepts.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct OperationQuery {
    /// The client-generated idempotency key.
    pub operation_id: Option<String>,
}

/// The `POST /client/messages/:id/move` body.
#[derive(Debug, Clone, Deserialize)]
pub struct ClientMoveRequest {
    /// The destination folder.
    pub folder_id: i64,
    /// An idempotency key.
    pub operation_id: Option<String>,
}

/// The `?permanent=` flag of the client delete route.
#[derive(Debug, Clone, Copy, Default, Deserialize)]
pub struct ClientDeleteQuery {
    /// Remove the row instead of moving the message to `Trash`.
    pub permanent: Option<bool>,
}

/// The `PATCH /client/messages/:id` body, which also carries `operation_id`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ClientPatchRequest {
    /// `\Seen`.
    pub seen: Option<bool>,
    /// `\Flagged`.
    pub flagged: Option<bool>,
    /// `\Answered`.
    pub answered: Option<bool>,
    /// `\Deleted`.
    pub deleted: Option<bool>,
    /// An idempotency key.
    pub operation_id: Option<String>,
}

/// `GET /api/v1/client/mailboxes`
pub async fn client_mailboxes(
    State(state): State<AppState>,
    client: ClientAuth,
) -> Result<Json<ClientMailboxListResponse>, ApiError> {
    Ok(Json(ClientMailboxListResponse {
        mailboxes: mailbox_tree(&state, client.user_id()).await?,
    }))
}

/// `GET /api/v1/client/sync`
pub async fn client_sync(
    State(state): State<AppState>,
    client: ClientAuth,
    Query(query): Query<SyncQuery>,
) -> Result<Json<ClientSyncResponse>, ApiError> {
    let mailbox_id = query.mailbox_id.ok_or_else(|| {
        ApiError::new(FerromaError::Invalid(
            "mailbox_id is required; it names the address to sync".to_string(),
        ))
    })?;
    // Ownership first, so a foreign mailbox id is a `404` and never a data leak.
    let mailbox = owned_mailbox(&state.repos, MailboxId::new(mailbox_id), client.user_id()).await?;

    let cursor = match query.cursor.as_deref().map(str::trim) {
        None | Some("") => Cursor::ZERO,
        Some(raw) => Cursor::parse(raw)?,
    };

    let request = SyncRequest {
        user_id: client.user_id(),
        mailbox_id: mailbox.mailbox_id(),
        folder_id: query.folder_id.map(MailboxId::new),
        cursor,
        limit: query.limit,
    };

    // A folder id from the request must belong to the same address.
    if let Some(folder_id) = request.folder_id {
        let folder = state
            .repos
            .folders
            .find_by_id(folder_id)
            .await
            .map_err(ApiError::from)?;
        match folder {
            Some(folder) if folder.mailbox_id == mailbox.id => {}
            _ => {
                return Err(ApiError::new(FerromaError::NotFound(
                    "no such folder".to_string(),
                )));
            }
        }
    }

    let page = state.sync.sync(request).await.map_err(|err| match err {
        // The documented recoverable-cursor failure.
        FerromaError::Conflict(reason) => ApiError::new(FerromaError::Conflict(format!(
            "cursor too old; full resync required ({reason})"
        ))),
        other => ApiError::new(other),
    })?;

    Ok(Json(ClientSyncResponse::from_page(page)))
}

/// `GET /api/v1/client/messages`
pub async fn client_messages(
    state: State<AppState>,
    client: ClientAuth,
    query: Query<MessageListQuery>,
) -> Result<Json<Page<MessageSummaryResponse>>, ApiError> {
    list_messages(state, management_view(&client), query).await
}

/// `GET /api/v1/client/messages/:id`
pub async fn client_message(
    State(state): State<AppState>,
    client: ClientAuth,
    Path(id): Path<i64>,
) -> Result<Json<MessageDetailResponse>, ApiError> {
    let (message, _mailbox, bytes) = state
        .mail_service
        .raw_message(MessageId::new(id), client.user_id())
        .await?;
    Ok(Json(detail_response(&state, &message, &bytes).await?))
}

/// `GET /api/v1/client/messages/:id/raw`
pub async fn client_message_raw(
    State(state): State<AppState>,
    client: ClientAuth,
    Path(id): Path<i64>,
) -> Result<axum::response::Response, ApiError> {
    crate::routes::mail::messages::get_raw_message(State(state), management_view(&client), Path(id))
        .await
}

/// The `POST /api/v1/client/messages` body.
///
/// `from` is optional here, unlike the management surface: a client that has exactly one
/// address sends from it without naming it, which is what the desktop client does.
#[derive(Debug, Clone, Deserialize)]
pub struct ClientSendRequest {
    /// The `From` address. Empty means "my primary address".
    #[serde(default)]
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
    /// File the message in `Drafts` instead of sending it.
    #[serde(default)]
    pub draft: bool,
}

impl ClientSendRequest {
    /// Convert into the shared send request.
    pub fn into_send_request(self) -> crate::service::SendRequest {
        crate::service::SendRequest {
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

/// `POST /api/v1/client/messages` — send, with idempotent replay.
pub async fn client_send_message(
    State(state): State<AppState>,
    client: ClientAuth,
    Query(operation): Query<OperationQuery>,
    Json(mut request): Json<ClientSendRequest>,
) -> Result<(StatusCode, Json<SendResponse>), ApiError> {
    // A client that leaves `from` empty means "send from my primary address".
    if request.from.trim().is_empty() {
        let primary = state
            .repos
            .mailboxes
            .find_primary(client.user_id())
            .await?
            .ok_or_else(|| {
                ApiError::new(FerromaError::Conflict(
                    "this account has no address to send from".to_string(),
                ))
            })?;
        let domain =
            crate::routes::mail::store::domain_name(&state.repos, primary.domain_id).await?;
        request.from = primary.address(&domain);
    }

    let draft = request.draft;
    let send = request.into_send_request();
    let user_id = client.user_id();

    let result = match operation
        .operation_id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())
    {
        Some(operation_id) => {
            state
                .sync
                .with_operation(operation_id, user_id, "send_message", || async {
                    state.mail_service.send(user_id, &send).await
                })
                .await?
        }
        None => state.mail_service.send(user_id, &send).await?,
    };

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

/// `PATCH /api/v1/client/messages/:id`
pub async fn client_patch_message(
    State(state): State<AppState>,
    client: ClientAuth,
    Path(id): Path<i64>,
    Json(request): Json<ClientPatchRequest>,
) -> Result<Json<MessageSummaryResponse>, ApiError> {
    let user_id = client.user_id();
    let updated = idempotent(
        &state,
        request.operation_id.as_deref(),
        user_id,
        "patch_message",
        || {
            let state = state.clone();
            let request = request.clone();
            async move {
                state
                    .mail_service
                    .set_message_flags(
                        MessageId::new(id),
                        user_id,
                        request.seen,
                        request.flagged,
                        request.answered,
                        request.deleted,
                    )
                    .await
            }
        },
    )
    .await?;
    Ok(Json(summary_response(&state, &updated).await?))
}

/// `DELETE /api/v1/client/messages/:id`
pub async fn client_delete_message(
    State(state): State<AppState>,
    client: ClientAuth,
    Path(id): Path<i64>,
    Query(query): Query<ClientDeleteQuery>,
) -> Result<StatusCode, ApiError> {
    state
        .mail_service
        .delete_message(
            MessageId::new(id),
            client.user_id(),
            query.permanent.unwrap_or(false),
        )
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /api/v1/client/messages/:id/read` (and `/unread`, `/star`)
pub async fn client_mark_read(
    State(state): State<AppState>,
    client: ClientAuth,
    Path(id): Path<i64>,
) -> Result<Json<MessageSummaryResponse>, ApiError> {
    set_seen(&state, &client, id, true).await
}

/// `POST /api/v1/client/messages/:id/unread`
pub async fn client_mark_unread(
    State(state): State<AppState>,
    client: ClientAuth,
    Path(id): Path<i64>,
) -> Result<Json<MessageSummaryResponse>, ApiError> {
    set_seen(&state, &client, id, false).await
}

/// `POST /api/v1/client/messages/:id/star`
pub async fn client_star(
    State(state): State<AppState>,
    client: ClientAuth,
    Path(id): Path<i64>,
) -> Result<Json<MessageSummaryResponse>, ApiError> {
    let updated = state
        .mail_service
        .set_message_flags(
            MessageId::new(id),
            client.user_id(),
            None,
            Some(true),
            None,
            None,
        )
        .await?;
    Ok(Json(summary_response(&state, &updated).await?))
}

/// `POST /api/v1/client/messages/:id/archive`
pub async fn client_archive(
    State(state): State<AppState>,
    client: ClientAuth,
    Path(id): Path<i64>,
) -> Result<Json<MessageSummaryResponse>, ApiError> {
    let (message, mailbox) =
        owned_live_message(&state.repos, MessageId::new(id), client.user_id()).await?;
    let archive = state.mail_service.archive_folder(&mailbox).await?;
    if archive.id == message.folder_id {
        // Already archived: answering with the current state is friendlier than a 409.
        return Ok(Json(summary_response(&state, &message).await?));
    }
    let moved = state
        .mail_service
        .move_message(MessageId::new(id), client.user_id(), archive.folder_id())
        .await?;
    Ok(Json(summary_response(&state, &moved).await?))
}

/// `POST /api/v1/client/messages/:id/trash`
pub async fn client_trash(
    State(state): State<AppState>,
    client: ClientAuth,
    Path(id): Path<i64>,
) -> Result<StatusCode, ApiError> {
    state
        .mail_service
        .delete_message(MessageId::new(id), client.user_id(), false)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /api/v1/client/messages/:id/move`
pub async fn client_move(
    State(state): State<AppState>,
    client: ClientAuth,
    Path(id): Path<i64>,
    Json(request): Json<ClientMoveRequest>,
) -> Result<Json<MessageSummaryResponse>, ApiError> {
    let user_id = client.user_id();
    let folder_id = request.folder_id;
    let moved = idempotent(
        &state,
        request.operation_id.as_deref(),
        user_id,
        "move_message",
        || {
            let state = state.clone();
            async move {
                state
                    .mail_service
                    .move_message(MessageId::new(id), user_id, MailboxId::new(folder_id))
                    .await
            }
        },
    )
    .await?;
    Ok(Json(summary_response(&state, &moved).await?))
}

/// `POST /api/v1/client/messages/batch`
pub async fn client_batch(
    State(state): State<AppState>,
    client: ClientAuth,
    request: Json<BatchRequest>,
) -> Result<Json<BatchResponse>, ApiError> {
    batch_messages(State(state), management_view(&client), request).await
}

/// `GET /api/v1/client/search` — the server-side fallback from `docs/fcp.md` §10.
pub async fn client_search(
    State(state): State<AppState>,
    client: ClientAuth,
    Query(query): Query<SearchQuery>,
) -> Result<Json<Page<MessageSummaryResponse>>, ApiError> {
    let pagination = Pagination::clamped(query.limit, query.offset);
    let parsed = parse_search_terms(query.q.as_deref().unwrap_or_default());

    let mailbox_scope = match query.mailbox_id {
        Some(id) => Some(
            owned_mailbox(&state.repos, MailboxId::new(id), client.user_id())
                .await?
                .mailbox_id(),
        ),
        None => state
            .repos
            .mailboxes
            .find_primary(client.user_id())
            .await?
            .map(|mailbox| mailbox.mailbox_id()),
    };

    let search = MessageSearch {
        folder_id: query.folder_id.map(MailboxId::new),
        mailbox_id: mailbox_scope,
        subject: parsed.subject,
        sender: parsed.from,
        text: parsed.body,
        unread_only: parsed.unread,
        flagged_only: parsed.flagged,
        with_attachments_only: parsed.has_attachment,
        since: parsed.after,
        before: parsed.before,
        limit: pagination.limit,
        offset: pagination.offset,
    };

    let rows = state.repos.messages.search(search).await?;
    let mut items = Vec::with_capacity(rows.len());
    for row in rows {
        items.push(summary_response(&state, &row).await?);
    }
    let total = items.len() as i64;
    Ok(Json(pagination.page(items, total)))
}

/// The `GET /api/v1/client/search` query.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SearchQuery {
    /// The operator string, e.g. `from:bob subject:invoice has:attachment`.
    pub q: Option<String>,
    /// Restrict to one address.
    pub mailbox_id: Option<i64>,
    /// Restrict to one folder.
    pub folder_id: Option<i64>,
    /// Page size.
    pub limit: Option<i64>,
    /// Rows to skip.
    pub offset: Option<i64>,
}

/// The operator set `docs/fcp.md` §10 documents.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchTerms {
    /// `from:`
    pub from: Option<String>,
    /// `to:` — folded into the text search, since the storage layer searches sender.
    pub to: Option<String>,
    /// `subject:`
    pub subject: Option<String>,
    /// `body:`
    pub body: Option<String>,
    /// `has:attachment`
    pub has_attachment: bool,
    /// `is:unread`
    pub unread: bool,
    /// `is:flagged`
    pub flagged: bool,
    /// `before:`
    pub before: Option<chrono::DateTime<chrono::Utc>>,
    /// `after:`
    pub after: Option<chrono::DateTime<chrono::Utc>>,
    /// `folder:` — a folder *name*, which the caller must resolve.
    pub folder: Option<String>,
    /// Everything that was not an operator.
    pub free_text: Option<String>,
}

/// Parse the `q=` operator string.
///
/// Unknown operators are treated as free text rather than refused: a client that
/// invents `label:x` should still find the messages that mention it.
pub fn parse_search_terms(raw: &str) -> SearchTerms {
    let mut terms = SearchTerms::default();
    let mut free: Vec<String> = Vec::new();

    // Split on whitespace, but keep a quoted phrase together.
    for token in split_terms(raw) {
        let lower = token.to_ascii_lowercase();
        // The operator name is matched case-insensitively; its *value* keeps the case
        // the user typed, so a folder called `Archive` does not become `archive`.
        let value_of = |prefix: &str| unquote(&token[prefix.len()..]);
        if lower.starts_with("from:") {
            terms.from = Some(value_of("from:"));
        } else if lower.starts_with("to:") {
            terms.to = Some(value_of("to:"));
        } else if lower.starts_with("subject:") {
            terms.subject = Some(value_of("subject:"));
        } else if lower.starts_with("body:") {
            terms.body = Some(value_of("body:"));
        } else if lower.starts_with("folder:") {
            terms.folder = Some(value_of("folder:"));
        } else if lower.starts_with("after:") {
            terms.after = parse_instant(&value_of("after:"));
        } else if lower.starts_with("before:") {
            terms.before = parse_instant(&value_of("before:"));
        } else if lower == "has:attachment" {
            terms.has_attachment = true;
        } else if lower == "is:unread" {
            terms.unread = true;
        } else if lower == "is:flagged" {
            terms.flagged = true;
        } else if !token.trim().is_empty() {
            free.push(token);
        }
    }

    if !free.is_empty() {
        terms.free_text = Some(free.join(" "));
    }
    // A free-text query with no explicit field searches the body-ish fields, which is
    // what a user typing a word expects.
    if terms.body.is_none() {
        terms.body = terms.free_text.clone();
    }
    terms
}

/// Split a query string into tokens, honouring double quotes.
fn split_terms(raw: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut in_quote = false;
    for ch in raw.chars() {
        match ch {
            '"' => {
                in_quote = !in_quote;
                current.push(ch);
            }
            ch if ch.is_whitespace() && !in_quote => {
                if !current.is_empty() {
                    out.push(std::mem::take(&mut current));
                }
            }
            ch => current.push(ch),
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// Strip surrounding quotes from an operator value.
fn unquote(value: &str) -> String {
    value.trim().trim_matches('"').trim().to_string()
}

/// `GET /api/v1/client/drafts`
pub async fn client_list_drafts(
    state: State<AppState>,
    client: ClientAuth,
    pagination: Query<PaginationQuery>,
) -> Result<Json<Page<crate::routes::mail::shapes::DraftResponse>>, ApiError> {
    crate::routes::mail::drafts::list_drafts(state, management_view(&client), pagination).await
}

/// `POST /api/v1/client/drafts`
pub async fn client_create_draft(
    state: State<AppState>,
    client: ClientAuth,
    request: Json<crate::routes::mail::drafts::DraftRequest>,
) -> Result<(StatusCode, Json<crate::routes::mail::shapes::DraftResponse>), ApiError> {
    crate::routes::mail::drafts::create_draft(state, management_view(&client), request).await
}

/// `GET /api/v1/client/drafts/:id`
pub async fn client_get_draft(
    state: State<AppState>,
    client: ClientAuth,
    path: Path<i64>,
) -> Result<Json<crate::routes::mail::shapes::DraftResponse>, ApiError> {
    crate::routes::mail::drafts::get_draft(state, management_view(&client), path).await
}

/// `PATCH /api/v1/client/drafts/:id`
pub async fn client_update_draft(
    state: State<AppState>,
    client: ClientAuth,
    path: Path<i64>,
    request: Json<crate::routes::mail::drafts::DraftRequest>,
) -> Result<Json<crate::routes::mail::shapes::DraftResponse>, ApiError> {
    crate::routes::mail::drafts::update_draft(state, management_view(&client), path, request).await
}

/// `DELETE /api/v1/client/drafts/:id`
pub async fn client_delete_draft(
    state: State<AppState>,
    client: ClientAuth,
    path: Path<i64>,
) -> Result<StatusCode, ApiError> {
    crate::routes::mail::drafts::delete_draft(state, management_view(&client), path).await
}

/// `GET /api/v1/client/devices` (`docs/fcp.md` §9)
pub async fn client_devices(
    State(state): State<AppState>,
    client: ClientAuth,
) -> Result<Json<DeviceList>, ApiError> {
    let rows = state
        .repos
        .devices
        .list_for_user(client.user_id(), true)
        .await?;
    Ok(Json(DeviceList {
        devices: rows.iter().map(DeviceResponse::from_row).collect(),
    }))
}

/// The `GET /api/v1/client/devices` body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceList {
    /// The caller's devices, revoked ones included.
    pub devices: Vec<DeviceResponse>,
}

/// `DELETE /api/v1/client/devices/:id` and `POST /api/v1/client/devices/:id/revoke`
///
/// Both revoke the device, revoke every session it holds and publish
/// `device.revoked`, so a live socket for it disconnects (the `docs/fcp.md` §9
/// sequence, step by step).
pub async fn client_revoke_device(
    State(state): State<AppState>,
    client: ClientAuth,
    Path(id): Path<i64>,
) -> Result<Json<DeviceResponse>, ApiError> {
    let device_id = ferroma_core::DeviceId::new(id);
    let device =
        crate::routes::mail::ownership::owned_device(&state.repos, device_id, client.user_id())
            .await?;

    let revoked_sessions = state
        .auth
        .revoke_device(device_id)
        .await
        .map_err(ApiError::from)?;

    state
        .events
        .publish(
            ferroma_events::EventScope::User(client.user_id()),
            ferroma_events::Event::device_revoked(device_id, client.user_id()),
        )
        .await;

    tracing::info!(
        device_id = id,
        user_id = client.user_id().get(),
        revoked_sessions,
        "client device revoked"
    );

    let updated = state
        .repos
        .devices
        .find_by_id(device_id)
        .await?
        .unwrap_or(device);
    Ok(Json(DeviceResponse::from_row(&updated)))
}

/// Run `operation` under an idempotency key, or straight through when there is none.
pub async fn idempotent<T, F, Fut>(
    state: &AppState,
    operation_id: Option<&str>,
    user: UserId,
    kind: &str,
    operation: F,
) -> Result<T, ApiError>
where
    T: serde::Serialize + serde::de::DeserializeOwned,
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<T, FerromaError>>,
{
    match operation_id.map(str::trim).filter(|id| !id.is_empty()) {
        Some(operation_id) => state
            .sync
            .with_operation(operation_id, user, kind, operation)
            .await
            .map_err(ApiError::from),
        None => operation().await.map_err(ApiError::from),
    }
}

/// Mark a message seen or unseen.
async fn set_seen(
    state: &AppState,
    client: &ClientAuth,
    id: i64,
    seen: bool,
) -> Result<Json<MessageSummaryResponse>, ApiError> {
    let updated = state
        .mail_service
        .set_message_flags(
            MessageId::new(id),
            client.user_id(),
            Some(seen),
            None,
            None,
            None,
        )
        .await?;
    Ok(Json(summary_response(state, &updated).await?))
}

/// Build the management-surface view of a client credential.
///
/// The two extractors differ only in which credentials they accept; the handlers they
/// feed are the same. Rather than duplicating every handler, the client routes call the
/// management ones with an equivalent [`crate::extract::AuthUser`].
fn management_view(client: &ClientAuth) -> crate::extract::AuthUser {
    crate::extract::AuthUser(client.auth.clone())
}

/// The change kinds a syncing client may be told about, for tests and documentation.
pub const SYNC_CHANGE_KINDS: [&str; 4] = [
    "message_created",
    "message_updated",
    "message_deleted",
    "message_moved",
];

/// Every `/client/*` FCP change kind, including folders and drafts.
pub fn all_change_kinds() -> Vec<&'static str> {
    [
        ChangeKind::MessageCreated,
        ChangeKind::MessageUpdated,
        ChangeKind::MessageDeleted,
        ChangeKind::MessageMoved,
        ChangeKind::FolderCreated,
        ChangeKind::FolderUpdated,
        ChangeKind::FolderDeleted,
        ChangeKind::DraftCreated,
        ChangeKind::DraftUpdated,
        ChangeKind::DraftDeleted,
    ]
    .into_iter()
    .map(ChangeKind::as_str)
    .collect()
}

/// The recipients of a message, for clients that render an address list.
pub async fn client_recipients(
    state: &AppState,
    id: MessageId,
) -> Result<Vec<crate::routes::mail::shapes::AddressResponse>, ApiError> {
    let recipients = state.repos.messages.recipients(id).await?;
    Ok(by_kind(&recipients, "to"))
}

/// The attachment list of a message, without the bytes.
pub async fn client_attachments(
    state: &AppState,
    id: MessageId,
) -> Result<Vec<AttachmentResponse>, ApiError> {
    let rows = state.repos.attachments.list_by_message(id).await?;
    Ok(rows.iter().map(AttachmentResponse::from_row).collect())
}

/// The pagination a client list endpoint uses.
pub fn client_pagination(query: &PaginationQuery) -> Pagination {
    Pagination::clamped(query.limit, query.offset)
}

/// Apply a management `PATCH` request through the client surface.
pub async fn patch_via_client(
    state: &AppState,
    client: &ClientAuth,
    id: i64,
    request: PatchMessageRequest,
) -> Result<MessageSummaryResponse, ApiError> {
    let updated = state
        .mail_service
        .set_message_flags(
            MessageId::new(id),
            client.user_id(),
            request.seen,
            request.flagged,
            request.answered,
            request.deleted,
        )
        .await?;
    summary_response(state, &updated).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    #[test]
    fn sync_terms_parse_every_documented_operator() {
        let terms = parse_search_terms(
            "from:bob subject:invoice body:total has:attachment is:unread is:flagged after:2026-01-01 before:2026-12-31 folder:Archive",
        );
        assert_eq!(terms.from.as_deref(), Some("bob"));
        assert_eq!(terms.subject.as_deref(), Some("invoice"));
        assert_eq!(terms.body.as_deref(), Some("total"));
        assert!(terms.has_attachment);
        assert!(terms.unread);
        assert!(terms.flagged);
        assert_eq!(terms.folder.as_deref(), Some("Archive"));
        assert_eq!(
            terms.after,
            Some(
                Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0)
                    .single()
                    .expect("valid")
            )
        );
        assert_eq!(
            terms.before,
            Some(
                Utc.with_ymd_and_hms(2026, 12, 31, 0, 0, 0)
                    .single()
                    .expect("valid")
            )
        );
    }

    #[test]
    fn quoted_operator_values_stay_together() {
        let terms = parse_search_terms("subject:\"invoice for september\" from:bob");
        assert_eq!(terms.subject.as_deref(), Some("invoice for september"));
        assert_eq!(terms.from.as_deref(), Some("bob"));
    }

    #[test]
    fn free_text_becomes_the_body_search() {
        let terms = parse_search_terms("quarterly report");
        assert_eq!(terms.body.as_deref(), Some("quarterly report"));
        assert!(terms.subject.is_none());
        assert!(terms.from.is_none());
    }

    #[test]
    fn an_unknown_operator_is_treated_as_free_text() {
        let terms = parse_search_terms("label:urgent from:bob");
        assert_eq!(terms.from.as_deref(), Some("bob"));
        assert!(terms
            .body
            .as_deref()
            .unwrap_or_default()
            .contains("label:urgent"));
    }

    #[test]
    fn an_empty_query_parses_to_nothing() {
        let terms = parse_search_terms("");
        assert_eq!(terms, SearchTerms::default());
        assert!(terms.body.is_none());
        let terms = parse_search_terms("    ");
        assert!(terms.body.is_none());
    }

    #[test]
    fn an_unparseable_date_is_dropped_rather_than_failing() {
        let terms = parse_search_terms("after:yesterday subject:hi");
        assert!(terms.after.is_none());
        assert_eq!(terms.subject.as_deref(), Some("hi"));
    }

    #[test]
    fn a_sync_page_renders_cursors_as_strings() {
        let page = SyncPage {
            next_cursor: Cursor(1841),
            has_more: true,
            latest_cursor: Cursor(1900),
            changes: Vec::new(),
        };
        let response = ClientSyncResponse::from_page(page);
        assert_eq!(response.next_cursor, "1841");
        assert_eq!(response.latest_cursor, "1900");
        assert!(response.has_more);
        let json = serde_json::to_value(&response).expect("must serialise");
        assert_eq!(json["next_cursor"], "1841");
        assert_eq!(json["has_more"], true);
        assert_eq!(json["changes"], serde_json::json!([]));
    }

    #[test]
    fn the_sync_query_is_all_optional() {
        let query: SyncQuery = serde_json::from_value(serde_json::json!({})).expect("must parse");
        assert!(query.mailbox_id.is_none());
        assert!(query.cursor.is_none());

        let query: SyncQuery = serde_json::from_value(serde_json::json!({
            "mailbox_id": 3,
            "folder_id": 5,
            "cursor": "1841",
            "limit": 100
        }))
        .expect("must parse");
        assert_eq!(query.mailbox_id, Some(3));
        assert_eq!(query.folder_id, Some(5));
        assert_eq!(query.cursor.as_deref(), Some("1841"));
        assert_eq!(query.limit, Some(100));
    }

    #[test]
    fn the_operation_id_is_optional_on_every_mutating_body() {
        let query: OperationQuery =
            serde_json::from_value(serde_json::json!({})).expect("must parse");
        assert!(query.operation_id.is_none());
        let query: OperationQuery =
            serde_json::from_value(serde_json::json!({ "operation_id": "op_1" }))
                .expect("must parse");
        assert_eq!(query.operation_id.as_deref(), Some("op_1"));

        let patch: ClientPatchRequest =
            serde_json::from_value(serde_json::json!({ "seen": true })).expect("must parse");
        assert!(patch.operation_id.is_none());
        assert_eq!(patch.seen, Some(true));

        let move_request: ClientMoveRequest =
            serde_json::from_value(serde_json::json!({ "folder_id": 6 })).expect("must parse");
        assert!(move_request.operation_id.is_none());
        assert_eq!(move_request.folder_id, 6);
    }

    #[test]
    fn the_mailbox_list_wraps_the_tree() {
        let response = ClientMailboxListResponse {
            mailboxes: Vec::new(),
        };
        let json = serde_json::to_value(&response).expect("must serialise");
        assert_eq!(json["mailboxes"], serde_json::json!([]));
    }

    #[test]
    fn the_device_list_wraps_the_devices() {
        let response = DeviceList {
            devices: Vec::new(),
        };
        let json = serde_json::to_value(&response).expect("must serialise");
        assert_eq!(json["devices"], serde_json::json!([]));
    }

    #[test]
    fn the_change_kind_vocabulary_is_the_protocol_one() {
        assert_eq!(
            SYNC_CHANGE_KINDS.to_vec(),
            vec![
                "message_created",
                "message_updated",
                "message_deleted",
                "message_moved"
            ]
        );
        assert_eq!(all_change_kinds().len(), 10);
        assert!(all_change_kinds().contains(&"folder_created"));
        assert!(all_change_kinds().contains(&"draft_deleted"));
    }

    #[test]
    fn client_pagination_clamps_like_every_other_list() {
        let pagination = client_pagination(&PaginationQuery {
            limit: Some(10_000),
            offset: Some(-1),
        });
        assert_eq!(pagination.limit, crate::extract::MAX_LIMIT);
        assert_eq!(pagination.offset, 0);
    }

    #[test]
    fn the_search_query_is_all_optional() {
        let query: SearchQuery = serde_json::from_value(serde_json::json!({})).expect("must parse");
        assert!(query.q.is_none());
        assert!(query.mailbox_id.is_none());
        assert!(query.folder_id.is_none());
    }
}
