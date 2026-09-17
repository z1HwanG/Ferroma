//! `docs/api.md` §5.3 — server-side drafts.
//!
//! A draft is two records that must never disagree:
//!
//! * a `drafts` row, which holds the editable JSON the Webmail compose window and the
//!   desktop client both autosave into, and which is what `GET /drafts` returns; and
//! * a real message in the address's `Drafts` folder carrying `\Draft`, so an IMAP
//!   client — Thunderbird, a phone — sees the same draft.
//!
//! Creating, updating and deleting always touch both, and deleting from either surface
//! removes both, which is what `docs/fcp.md` §7 requires.
//!
//! The recipient and attachment columns are `JSONB`, so the shapes are documented here
//! and parsed defensively: a row written by an older build (or by hand) must not be
//! able to make a `GET` panic.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use ferroma_core::{DraftId, FerromaError, MailboxId, MessageId};
use ferroma_storage::repository::{DraftUpdate, NewDraft};
use serde::{Deserialize, Serialize};

use crate::error::ApiError;
use crate::extract::{AuthUser, Pagination, Page};
use crate::routes::mail::ownership::{owned_draft, owned_mailbox, owned_message};
use crate::routes::mail::shapes::{AddressResponse, AttachmentResponse, DraftResponse};
use crate::service::SendRequest;
use crate::state::AppState;

/// The `POST /api/v1/drafts` body, and the `PATCH` body with every field optional.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct DraftRequest {
    /// The address the draft will be sent from.
    pub mailbox_id: Option<i64>,
    /// Subject.
    pub subject: Option<String>,
    /// Plain-text body.
    pub text: Option<String>,
    /// HTML body.
    pub html: Option<String>,
    /// `To` recipients.
    pub to: Option<Vec<String>>,
    /// `Cc` recipients.
    pub cc: Option<Vec<String>>,
    /// `Bcc` recipients.
    pub bcc: Option<Vec<String>>,
    /// The message being replied to.
    pub in_reply_to: Option<String>,
    /// The reference chain.
    pub references: Option<Vec<String>>,
    /// Attachment ids, or `"<id>:<filename>"` strings.
    pub attachment_ids: Option<Vec<serde_json::Value>>,
}

/// One stored recipient of a draft.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DraftRecipient {
    /// `to`, `cc` or `bcc`.
    pub kind: String,
    /// The address.
    pub address: String,
    /// The display name, when there was one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// One attachment of a draft.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DraftAttachment {
    /// The attachment row id, when the draft still references a live upload.
    pub id: Option<i64>,
    /// The file name.
    pub filename: String,
    /// The MIME type.
    #[serde(default = "default_content_type")]
    pub content_type: String,
    /// Size in bytes, when known.
    #[serde(default)]
    pub size_bytes: i64,
    /// The blob path, which the send path needs and the JSON does not show.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_path: Option<String>,
}

/// The content type an attachment with no recorded type gets.
fn default_content_type() -> String {
    "application/octet-stream".to_string()
}

/// `GET /api/v1/drafts`
pub async fn list_drafts(
    State(state): State<AppState>,
    user: AuthUser,
    Query(pagination): Query<crate::extract::PaginationQuery>,
) -> Result<Json<Page<DraftResponse>>, ApiError> {
    let page = Pagination::clamped(pagination.limit, pagination.offset);
    let rows = state
        .repos
        .drafts
        .list_for_user(user.user_id(), page.limit, page.offset)
        .await?;
    let total = state
        .repos
        .drafts
        .count_for_user(user.user_id())
        .await?;
    let items = rows.iter().map(draft_response).collect();
    Ok(Json(page.page(items, total)))
}

/// `POST /api/v1/drafts`
pub async fn create_draft(
    State(state): State<AppState>,
    user: AuthUser,
    Json(request): Json<DraftRequest>,
) -> Result<(StatusCode, Json<DraftResponse>), ApiError> {
    let mailbox = resolve_draft_mailbox(&state, &user, request.mailbox_id).await?;
    let recipients = recipients_json(&request);
    let attachments = attachments_json(&state, &user, &request).await?;

    let row = state
        .repos
        .drafts
        .create(NewDraft {
            user_id: user.user_id(),
            mailbox_id: Some(mailbox.mailbox_id()),
            folder_id: None,
            subject: request.subject.clone(),
            body_text: request.text.clone(),
            body_html: request.html.clone(),
            recipients: serde_json::Value::Array(recipients),
            attachments: serde_json::Value::Array(attachments),
            in_reply_to: request.in_reply_to.clone(),
            reference_ids: serde_json::Value::Array(
                request
                    .references
                    .clone()
                    .unwrap_or_default()
                    .into_iter()
                    .map(serde_json::Value::String)
                    .collect(),
            ),
        })
        .await?;

    // Mirror it into the Drafts folder so an IMAP client sees it too.
    let mirrored = mirror_draft(&state, &user, &row, request.mailbox_id).await;
    let row = match mirrored {
        Some(message_id) => state
            .repos
            .drafts
            .update(
                row.draft_id(),
                DraftUpdate {
                    message_id: Some(message_id),
                    ..DraftUpdate::default()
                },
            )
            .await
            .unwrap_or(row),
        None => row,
    };

    publish_draft(&state, &user, &row, false).await;
    Ok((StatusCode::CREATED, Json(draft_response(&row))))
}

/// `GET /api/v1/drafts/:id`
pub async fn get_draft(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
) -> Result<Json<DraftResponse>, ApiError> {
    let row = owned_draft(&state.repos, DraftId::new(id), user.user_id()).await?;
    Ok(Json(draft_response(&row)))
}

/// `PATCH /api/v1/drafts/:id`
pub async fn update_draft(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
    Json(request): Json<DraftRequest>,
) -> Result<Json<DraftResponse>, ApiError> {
    let existing = owned_draft(&state.repos, DraftId::new(id), user.user_id()).await?;

    let recipients = request
        .to
        .as_ref()
        .map(|_| recipients_json(&request))
        .map(serde_json::Value::Array);

    let attachments = if request.attachment_ids.is_some() {
        Some(serde_json::Value::Array(
            attachments_json(&state, &user, &request).await?,
        ))
    } else {
        None
    };

    let update = DraftUpdate {
        subject: request.subject.clone(),
        body_text: request.text.clone(),
        body_html: request.html.clone(),
        recipients,
        attachments,
        in_reply_to: request.in_reply_to.clone(),
        reference_ids: request.references.as_ref().map(|references| {
            serde_json::Value::Array(
                references
                    .iter()
                    .cloned()
                    .map(serde_json::Value::String)
                    .collect(),
            )
        }),
        message_id: None,
    };

    let row = state
        .repos
        .drafts
        .update(existing.draft_id(), update)
        .await?;

    // Re-mirror so the IMAP copy reflects the edit. The old mirror is replaced rather
    // than patched, which is simpler and cannot leave a half-updated message behind.
    let mailbox_id = row.mailbox_id.unwrap_or(0);
    if let Some(previous) = row.message_id {
        if let Ok((message, _mailbox)) =
            owned_message(&state.repos, MessageId::new(previous), user.user_id()).await
        {
            let _ = state.maildir.delete(&message.storage_path);
            let _ = state.repos.messages.hard_delete(message.message_id()).await;
        }
    }
    let mirrored = mirror_draft(&state, &user, &row, Some(mailbox_id)).await;
    let row = match mirrored {
        Some(message_id) => state
            .repos
            .drafts
            .update(
                row.draft_id(),
                DraftUpdate {
                    message_id: Some(message_id),
                    ..DraftUpdate::default()
                },
            )
            .await
            .unwrap_or(row),
        None => row,
    };

    publish_draft(&state, &user, &row, false).await;
    Ok(Json(draft_response(&row)))
}

/// `DELETE /api/v1/drafts/:id`
pub async fn delete_draft(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
) -> Result<StatusCode, ApiError> {
    let row = owned_draft(&state.repos, DraftId::new(id), user.user_id()).await?;

    if let Some(message_id) = row.message_id {
        if let Ok((message, _mailbox)) =
            owned_message(&state.repos, MessageId::new(message_id), user.user_id()).await
        {
            let _ = state.maildir.delete(&message.storage_path);
            let _ = state.repos.messages.hard_delete(message.message_id()).await;
        }
    }

    state
        .repos
        .drafts
        .delete(row.draft_id())
        .await
        .map_err(ApiError::from)?;

    publish_draft(&state, &user, &row, true).await;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /api/v1/drafts/:id/send` — the management surface's "send this draft".
pub async fn send_draft(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
    Json(mut request): Json<DraftRequest>,
) -> Result<Json<crate::routes::mail::shapes::SendResponse>, ApiError> {
    let row = owned_draft(&state.repos, DraftId::new(id), user.user_id()).await?;

    let mailbox = match row.mailbox_id {
        Some(mailbox_id) => {
            owned_mailbox(&state.repos, MailboxId::new(mailbox_id), user.user_id()).await?
        }
        None => resolve_draft_mailbox(&state, &user, request.mailbox_id).await?,
    };
    let domain = crate::routes::mail::store::domain_name(&state.repos, mailbox.domain_id).await?;

    // Anything the caller did not resupply comes from the stored draft.
    if request.subject.is_none() {
        request.subject = row.subject.clone();
    }
    if request.text.is_none() {
        request.text = row.body_text.clone();
    }
    if request.html.is_none() {
        request.html = row.body_html.clone();
    }
    if request.to.is_none() && request.cc.is_none() && request.bcc.is_none() {
        let stored = decode_recipients(&row);
        request.to = Some(
            stored
                .iter()
                .filter(|recipient| recipient.kind == "to")
                .map(|recipient| recipient.address.clone())
                .collect(),
        );
        request.cc = Some(
            stored
                .iter()
                .filter(|recipient| recipient.kind == "cc")
                .map(|recipient| recipient.address.clone())
                .collect(),
        );
        request.bcc = Some(
            stored
                .iter()
                .filter(|recipient| recipient.kind == "bcc")
                .map(|recipient| recipient.address.clone())
                .collect(),
        );
    }
    if request.attachment_ids.is_none() {
        request.attachment_ids = Some(
            decode_attachments(&row)
                .into_iter()
                .filter_map(|attachment| attachment.id.map(serde_json::Value::from))
                .collect(),
        );
    }

    let send = SendRequest {
        from: mailbox.address(&domain),
        from_name: None,
        to: request.to.clone().unwrap_or_default(),
        cc: request.cc.clone().unwrap_or_default(),
        bcc: request.bcc.clone().unwrap_or_default(),
        subject: request.subject.clone().unwrap_or_default(),
        text: request.text.clone(),
        html: request.html.clone(),
        attachment_ids: json_ids(&request.attachment_ids),
        in_reply_to: request.in_reply_to.clone().or_else(|| row.in_reply_to.clone()),
        references: request.references.clone().unwrap_or_else(|| {
            row.reference_ids
                .as_array()
                .map(|values| {
                    values
                        .iter()
                        .filter_map(|value| value.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default()
        }),
        reply_to: None,
        draft: false,
    };

    let result = state.mail_service.send(user.user_id(), &send).await?;

    // The draft has become a message; remove the draft and its mirror.
    if let Some(message_id) = row.message_id {
        if let Ok((message, _mailbox)) =
            owned_message(&state.repos, MessageId::new(message_id), user.user_id()).await
        {
            let _ = state.maildir.delete(&message.storage_path);
            let _ = state.repos.messages.hard_delete(message.message_id()).await;
        }
    }
    let _ = state.repos.drafts.delete(row.draft_id()).await;
    publish_draft(&state, &user, &row, true).await;

    Ok(Json(crate::routes::mail::shapes::SendResponse {
        message_id: result.message_id,
        queued: result.queued,
        recipients: result.recipients,
    }))
}

/// Which address a draft belongs to: the one asked for, or the primary one.
async fn resolve_draft_mailbox(
    state: &AppState,
    user: &AuthUser,
    requested: Option<i64>,
) -> Result<ferroma_storage::models::Mailbox, ApiError> {
    match requested {
        Some(id) => Ok(owned_mailbox(&state.repos, MailboxId::new(id), user.user_id()).await?),
        None => state
            .repos
            .mailboxes
            .find_primary(user.user_id())
            .await?
            .ok_or_else(|| {
                ApiError::new(FerromaError::Conflict(
                    "this account has no address yet".to_string(),
                ))
            }),
    }
}

/// The `recipients` JSON for a request.
fn recipients_json(request: &DraftRequest) -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    for (kind, values) in [
        ("to", request.to.as_ref()),
        ("cc", request.cc.as_ref()),
        ("bcc", request.bcc.as_ref()),
    ] {
        for value in values.into_iter().flatten() {
            for mailbox in ferroma_mail::address::parse_address_list(value) {
                out.push(serde_json::json!({
                    "kind": kind,
                    "address": mailbox.address.to_string(),
                    "name": mailbox.name,
                }));
            }
        }
    }
    out
}

/// The `attachments` JSON for a request, resolving each id's metadata.
async fn attachments_json(
    state: &AppState,
    user: &AuthUser,
    request: &DraftRequest,
) -> Result<Vec<serde_json::Value>, ApiError> {
    let Some(ids) = request.attachment_ids.as_ref() else {
        return Ok(Vec::new());
    };
    let mut out = Vec::with_capacity(ids.len());
    for value in ids {
        let id = match value {
            serde_json::Value::Number(number) => number.as_i64(),
            serde_json::Value::String(text) => text
                .split_once(':')
                .and_then(|(id, _)| id.trim().parse::<i64>().ok())
                .or_else(|| text.trim().parse::<i64>().ok()),
            _ => None,
        };
        let Some(id) = id else { continue };
        let row = crate::routes::mail::ownership::owned_attachment(
            &state.repos,
            ferroma_core::AttachmentId::new(id),
            user.user_id(),
        )
        .await?;
        out.push(serde_json::json!({
            "id": row.id,
            "filename": row.filename.clone().unwrap_or_else(|| format!("attachment-{id}")),
            "content_type": row.content_type,
            "size_bytes": row.size_bytes,
            "storage_path": row.storage_path,
        }));
    }
    Ok(out)
}

/// The id list out of a JSON attachment array.
fn json_ids(values: &Option<Vec<serde_json::Value>>) -> Vec<i64> {
    values
        .as_ref()
        .map(|values| {
            values
                .iter()
                .filter_map(|value| match value {
                    serde_json::Value::Number(number) => number.as_i64(),
                    serde_json::Value::String(text) => text
                        .split_once(':')
                        .and_then(|(id, _)| id.trim().parse::<i64>().ok())
                        .or_else(|| text.trim().parse::<i64>().ok()),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Read the `recipients` JSON defensively.
pub fn decode_recipients(row: &ferroma_storage::models::Draft) -> Vec<DraftRecipient> {
    row.recipients
        .as_array()
        .map(|values| {
            values
                .iter()
                .filter_map(|value| {
                    let address = value.get("address")?.as_str()?.to_string();
                    Some(DraftRecipient {
                        kind: value
                            .get("kind")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("to")
                            .to_string(),
                        address,
                        name: value
                            .get("name")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_string),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Read the `attachments` JSON defensively.
pub fn decode_attachments(row: &ferroma_storage::models::Draft) -> Vec<DraftAttachment> {
    row.attachments
        .as_array()
        .map(|values| {
            values
                .iter()
                .map(|value| {
                    // A legacy row may carry only `filename` + `storage_path`; a modern
                    // one carries an id too. Either way, never panic on junk.
                    let filename = value
                        .get("filename")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("attachment")
                        .to_string();
                    DraftAttachment {
                        id: value.get("id").and_then(serde_json::Value::as_i64),
                        filename,
                        content_type: value
                            .get("content_type")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("application/octet-stream")
                            .to_string(),
                        size_bytes: value
                            .get("size_bytes")
                            .and_then(serde_json::Value::as_i64)
                            .unwrap_or(0),
                        storage_path: value
                            .get("storage_path")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_string),
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Turn a stored draft into the documented response.
pub fn draft_response(row: &ferroma_storage::models::Draft) -> DraftResponse {
    let recipients = decode_recipients(row);
    let attachments = decode_attachments(row);
    let pick = |kind: &str| -> Vec<AddressResponse> {
        recipients
            .iter()
            .filter(|recipient| recipient.kind == kind)
            .map(|recipient| AddressResponse {
                address: recipient.address.clone(),
                name: recipient.name.clone(),
            })
            .collect()
    };
    DraftResponse {
        id: row.id,
        mailbox_id: row.mailbox_id,
        folder_id: row.folder_id,
        message_id: row.message_id,
        subject: row.subject.clone(),
        text: row.body_text.clone(),
        html: row.body_html.clone(),
        to: pick("to"),
        cc: pick("cc"),
        bcc: pick("bcc"),
        in_reply_to: row.in_reply_to.clone(),
        references: row
            .reference_ids
            .as_array()
            .map(|values| {
                values
                    .iter()
                    .filter_map(|value| value.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
        attachments: attachments
            .iter()
            .map(|attachment| AttachmentResponse {
                id: attachment.id.unwrap_or(0),
                filename: Some(attachment.filename.clone()),
                content_type: attachment.content_type.clone(),
                size_bytes: attachment.size_bytes,
                is_inline: false,
                content_id: None,
            })
            .collect(),
        created_at: row.created_at,
        updated_at: row.updated_at,
    }
}

/// Write the draft into the address's `Drafts` folder as a real message.
async fn mirror_draft(
    state: &AppState,
    user: &AuthUser,
    row: &ferroma_storage::models::Draft,
    mailbox_id: Option<i64>,
) -> Option<MessageId> {
    let mailbox_id = match mailbox_id.or(row.mailbox_id) {
        Some(id) => id,
        None => match state.repos.mailboxes.find_primary(user.user_id()).await {
            Ok(Some(mailbox)) => mailbox.id,
            _ => return None,
        },
    };
    let mailbox = match state.repos.mailboxes.find_by_id(MailboxId::new(mailbox_id)).await {
        Ok(Some(mailbox)) if mailbox.user_id == user.user_id().get() => mailbox,
        _ => return None,
    };
    let domain = crate::routes::mail::store::domain_name(&state.repos, mailbox.domain_id)
        .await
        .ok()?;
    if state
        .maildir
        .ensure_mailbox(&domain, &mailbox.local_part)
        .is_err()
    {
        return None;
    }
    let _ = state
        .repos
        .folders
        .ensure_standard(mailbox.mailbox_id())
        .await;

    let recipients = decode_recipients(row);
    let mut outgoing = crate::routes::mail::store::OutgoingMessage {
        from: mailbox.address(&domain),
        from_name: None,
        subject: row.subject.clone().unwrap_or_default(),
        text: row.body_text.clone(),
        html: row.body_html.clone(),
        in_reply_to: row.in_reply_to.clone(),
        references: row
            .reference_ids
            .as_array()
            .map(|values| {
                values
                    .iter()
                    .filter_map(|value| value.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
        ..crate::routes::mail::store::OutgoingMessage::default()
    };
    for recipient in &recipients {
        match recipient.kind.as_str() {
            "cc" => outgoing.cc.push(recipient.address.clone()),
            "bcc" => outgoing.bcc.push(recipient.address.clone()),
            _ => outgoing.to.push(recipient.address.clone()),
        }
    }

    let bytes = outgoing.build(&domain).ok()?;
    let body = crate::routes::mail::store::describe_built_message(&bytes, &outgoing);
    let stored = crate::routes::mail::store::store_message(
        &state.repos,
        &state.maildir,
        &domain,
        &mailbox,
        "Drafts",
        "draft seen",
        true,
        &outgoing,
        &body,
    )
    .await
    .ok()?;

    Some(stored.message.message_id())
}

/// Publish the draft change so other devices learn about it.
async fn publish_draft(
    state: &AppState,
    user: &AuthUser,
    row: &ferroma_storage::models::Draft,
    deleted: bool,
) {
    let event = if deleted {
        ferroma_events::Event::draft_updated(row.draft_id(), true)
    } else {
        ferroma_events::Event::draft_created(row.draft_id(), row.mailbox_id.map(MailboxId::new))
    };
    state
        .events
        .publish(ferroma_events::EventScope::User(user.user_id()), event)
        .await;
    let _ = state
        .sync
        .record_draft_change(
            user.user_id(),
            row.mailbox_id.map(MailboxId::new),
            row.draft_id(),
            row.subject.as_deref(),
            if deleted {
                ferroma_sync::ChangeKind::DraftDeleted
            } else {
                ferroma_sync::ChangeKind::DraftCreated
            },
        )
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draft(recipients: serde_json::Value, attachments: serde_json::Value) -> ferroma_storage::models::Draft {
        ferroma_storage::models::Draft {
            id: 44,
            user_id: 7,
            mailbox_id: Some(3),
            folder_id: Some(7),
            message_id: Some(99),
            subject: Some("hi".into()),
            body_text: Some("body".into()),
            body_html: Some("<p>body</p>".into()),
            recipients,
            attachments,
            in_reply_to: Some("<a@b.c>".into()),
            reference_ids: serde_json::json!(["<x@y.z>"]),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn recipients_decode_in_order_and_by_kind() {
        let json = serde_json::json!([
            { "kind": "to", "address": "bob@example.net", "name": "Bob" },
            { "kind": "cc", "address": "carol@example.org", "name": null },
            { "kind": "bcc", "address": "dan@example.io" }
        ]);
        let recipients = decode_recipients(&draft(json, serde_json::json!([])));
        assert_eq!(recipients.len(), 3);
        assert_eq!(recipients[0].kind, "to");
        assert_eq!(recipients[0].name.as_deref(), Some("Bob"));
        assert_eq!(recipients[1].kind, "cc");
        // A missing `kind` defaults to `to` rather than dropping the recipient.
        assert_eq!(recipients[2].kind, "bcc");
    }

    #[test]
    fn junk_recipients_never_panic() {
        assert!(decode_recipients(&draft(serde_json::json!(null), serde_json::json!([]))).is_empty());
        assert!(decode_recipients(&draft(serde_json::json!("nope"), serde_json::json!([]))).is_empty());
        assert!(decode_recipients(&draft(serde_json::json!([1, 2, {}]), serde_json::json!([]))).is_empty());
        assert!(decode_recipients(&draft(serde_json::json!([{ "address": 5 }]), serde_json::json!([]))).is_empty());
    }

    #[test]
    fn attachments_decode_with_and_without_an_id() {
        let json = serde_json::json!([
            { "id": 991, "filename": "a.pdf", "content_type": "application/pdf", "size_bytes": 10, "storage_path": "ab/cd/x" },
            { "filename": "legacy.bin", "storage_path": "ef/01/y" }
        ]);
        let attachments = decode_attachments(&draft(serde_json::json!([]), json));
        assert_eq!(attachments.len(), 2);
        assert_eq!(attachments[0].id, Some(991));
        assert_eq!(attachments[0].content_type, "application/pdf");
        assert_eq!(attachments[1].id, None);
        assert_eq!(attachments[1].content_type, "application/octet-stream");
        assert_eq!(attachments[1].filename, "legacy.bin");
    }

    #[test]
    fn junk_attachments_never_panic() {
        assert!(decode_attachments(&draft(serde_json::json!([]), serde_json::json!(null))).is_empty());
        assert!(decode_attachments(&draft(serde_json::json!([]), serde_json::json!([null, 3]))).len() == 2);
    }

    #[test]
    fn the_response_groups_recipients_and_reads_the_reference_chain() {
        let row = draft(
            serde_json::json!([
                { "kind": "to", "address": "bob@example.net" },
                { "kind": "cc", "address": "carol@example.org" }
            ]),
            serde_json::json!([{ "id": 1, "filename": "a.bin" }]),
        );
        let response = draft_response(&row);
        assert_eq!(response.id, 44);
        assert_eq!(response.mailbox_id, Some(3));
        assert_eq!(response.message_id, Some(99));
        assert_eq!(response.to.len(), 1);
        assert_eq!(response.cc.len(), 1);
        assert!(response.bcc.is_empty());
        assert_eq!(response.references, vec!["<x@y.z>"]);
        assert_eq!(response.attachments.len(), 1);
        assert_eq!(response.attachments[0].filename.as_deref(), Some("a.bin"));
    }

    #[test]
    fn a_draft_with_null_json_still_renders() {
        let row = draft(serde_json::json!(null), serde_json::json!(null));
        let response = draft_response(&row);
        assert!(response.to.is_empty());
        // The reference chain lives in its own column, so it survives a null recipient
        // and attachment column.
        assert_eq!(response.references, vec!["<x@y.z>".to_string()]);
        assert!(response.attachments.is_empty());
    }

    #[test]
    fn a_draft_with_a_junk_reference_column_renders_without_panicking() {
        let mut row = draft(serde_json::json!([]), serde_json::json!([]));
        row.reference_ids = serde_json::json!("not an array");
        let response = draft_response(&row);
        assert!(response.references.is_empty());
    }

    #[test]
    fn recipients_json_parses_display_names_and_groups() {
        let request = DraftRequest {
            to: Some(vec!["Bob <bob@example.net>, carol@example.org".into()]),
            cc: Some(vec!["dan@example.io".into()]),
            bcc: None,
            ..DraftRequest::default()
        };
        let json = recipients_json(&request);
        assert_eq!(json.len(), 3);
        assert_eq!(json[0]["kind"], "to");
        assert_eq!(json[0]["address"], "bob@example.net");
        assert_eq!(json[0]["name"], "Bob");
        assert_eq!(json[1]["address"], "carol@example.org");
        assert_eq!(json[2]["kind"], "cc");
    }

    #[test]
    fn json_ids_accept_numbers_and_the_documented_string_form() {
        let values = Some(vec![
            serde_json::Value::from(7),
            serde_json::Value::from("9:a.pdf"),
            serde_json::Value::from("11"),
            serde_json::Value::Bool(true),
        ]);
        assert_eq!(json_ids(&values), vec![7, 9, 11]);
        assert!(json_ids(&None).is_empty());
    }

    #[test]
    fn draft_request_defaults_to_nothing() {
        let request = DraftRequest::default();
        assert!(request.to.is_none());
        assert!(request.attachment_ids.is_none());
        assert!(request.subject.is_none());
    }

    #[test]
    fn draft_request_accepts_attachment_ids_as_numbers_or_strings() {
        let request: DraftRequest = serde_json::from_value(serde_json::json!({
            "subject": "hi",
            "attachment_ids": [1, "2:file.bin"]
        }))
        .expect("must deserialise");
        assert_eq!(json_ids(&request.attachment_ids), vec![1, 2]);
    }
}
