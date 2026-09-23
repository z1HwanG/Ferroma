//! JMAP RFC 8620 / RFC 8621 HTTP adapter.
//!
//! This module is intentionally an adapter over [`crate::service::MessageService`] and
//! the repositories. It does not create a second mail store: mutations use the same
//! service methods that record sync changes and publish mail events for IMAP and FCP.

use std::collections::BTreeSet;

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::{DateTime, Utc};
use ferroma_core::{FerromaError, MailboxId, MessageId};
use ferroma_storage::repository::MessageSearch;
use serde_json::{json, Map, Value};

use crate::error::ApiError;
use crate::extract::JmapAuth;
use crate::routes::mail::ownership::owned_attachment;
use crate::routes::mail::store::domain_name;
use crate::state::{AppState, MessageStub};

const CORE: &str = "urn:ietf:params:jmap:core";
const MAIL: &str = "urn:ietf:params:jmap:mail";
const MAX_CALLS: usize = 16;
const MAX_GET: usize = 500;
const MAX_SET: usize = 500;

/// Refuse the JMAP surface before authentication or mailbox work when an administrator
/// disabled it at runtime. The HTTP API itself remains available for the control plane.
fn require_enabled(state: &AppState) -> Result<(), ApiError> {
    if state.listeners.states().jmap.enabled {
        Ok(())
    } else {
        Err(ApiError::new(FerromaError::NotFound(
            "the JMAP service is disabled by the administrator".to_string(),
        )))
    }
}

/// `GET /.well-known/jmap`: return the authenticated RFC 8620 Session resource.
pub async fn session(State(state): State<AppState>, auth: JmapAuth) -> Result<Response, ApiError> {
    require_enabled(&state)?;
    let account = account_id(auth.user_id().get());
    let email = primary_address(&state, auth.user_id().get()).await?;
    let base = public_base(&state);
    let payload = json!({
        "capabilities": {
            CORE: {
                "maxSizeUpload": state.config.limits.max_attachment_size,
                "maxConcurrentUpload": 1,
                "maxSizeRequest": state.config.api.max_request_size,
                "maxConcurrentRequests": 4,
                "maxCallsInRequest": MAX_CALLS,
                "maxObjectsInGet": MAX_GET,
                "maxObjectsInSet": MAX_SET,
                "collationAlgorithms": []
            },
            MAIL: {}
        },
        "accounts": {
            &account: {
                "name": email,
                "isPersonal": true,
                "isReadOnly": false,
                "accountCapabilities": { MAIL: {
                    "maxMailboxesPerEmail": 1,
                    "maxMailboxDepth": null,
                    "maxSizeMailboxName": 255,
                    "maxSizeAttachmentsPerEmail": state.config.limits.max_attachment_size,
                    "emailQuerySortOptions": ["receivedAt", "sentAt", "size", "from", "subject"],
                    "mayCreateTopLevelMailbox": true
                }}
            }
        },
        "primaryAccounts": { MAIL: account },
        "username": auth.user().email,
        "apiUrl": format!("{base}/api/jmap/"),
        "downloadUrl": format!("{base}/api/jmap/download/{{accountId}}/{{blobId}}?type={{type}}&name={{name}}"),
        "uploadUrl": format!("{base}/api/jmap/upload/{{accountId}}"),
        "eventSourceUrl": "",
        "state": session_state(&state, auth.user_id().get()).await
    });
    let mut response = Json(payload).into_response();
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-cache, no-store, must-revalidate"),
    );
    Ok(response)
}

/// `POST /api/jmap`: dispatch supported JMAP method calls serially.
pub async fn api(
    State(state): State<AppState>,
    auth: JmapAuth,
    Json(request): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    require_enabled(&state)?;
    let object = request
        .as_object()
        .ok_or_else(|| invalid("JMAP request must be an object"))?;
    let using = object
        .get("using")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid("using must be an array"))?;
    if !using.iter().any(|value| value.as_str() == Some(CORE))
        || !using.iter().any(|value| value.as_str() == Some(MAIL))
    {
        return Err(invalid(
            "using must include JMAP core and mail capabilities",
        ));
    }
    let calls = object
        .get("methodCalls")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid("methodCalls must be an array"))?;
    if calls.len() > MAX_CALLS {
        return Err(invalid("too many method calls"));
    }
    let mut responses = Vec::with_capacity(calls.len());
    for call in calls {
        let Some(parts) = call.as_array() else {
            return Err(invalid("each method call must be an array"));
        };
        if parts.len() != 3 {
            return Err(invalid("each method call must have three elements"));
        }
        let (Some(method), Some(args), Some(call_id)) =
            (parts[0].as_str(), parts[1].as_object(), parts[2].as_str())
        else {
            return Err(invalid("invalid method call"));
        };
        let response = match dispatch(&state, &auth, method, args).await {
            Ok((name, body)) => json!([name, body, call_id]),
            Err((kind, description)) => {
                json!(["error", {"type": kind, "description": description}, call_id])
            }
        };
        responses.push(response);
    }
    let state_value = session_state(&state, auth.user_id().get()).await;
    Ok(Json(
        json!({"methodResponses": responses, "sessionState": state_value}),
    ))
}

/// Upload a raw JMAP blob. Uploaded blobs are held by the caller's existing attachment
/// placeholder until `Email/import` consumes them.
pub async fn upload(
    State(state): State<AppState>,
    auth: JmapAuth,
    Path(account): Path<String>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Response, ApiError> {
    require_enabled(&state)?;
    require_account(&account, auth.user_id().get()).map_err(ApiError::from)?;
    if body.len() as u64 > state.config.limits.max_attachment_size {
        return Err(ApiError::new(FerromaError::LimitExceeded(
            "JMAP upload exceeds the attachment size limit".to_string(),
        )));
    }
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
        .unwrap_or("application/octet-stream");
    let blob = state.attachments.store(&body)?;
    let stub = MessageStub::ensure(&state, auth.user_id()).await?;
    let row = state
        .repos
        .attachments
        .insert(
            stub.message_id,
            ferroma_storage::repository::NewAttachment {
                filename: Some("jmap-upload".to_string()),
                content_type: content_type.to_string(),
                size_bytes: blob.size as i64,
                storage_path: blob.path,
                content_id: None,
                is_inline: false,
                checksum_sha256: Some(blob.sha256),
            },
        )
        .await?;
    Ok((StatusCode::CREATED, Json(json!({"accountId": account, "blobId": attachment_blob_id(row.id), "type": content_type, "size": row.size_bytes}))).into_response())
}

/// Download a JMAP blob owned by the current account.
pub async fn download(
    State(state): State<AppState>,
    auth: JmapAuth,
    Path((account, blob_id)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    require_enabled(&state)?;
    require_account(&account, auth.user_id().get()).map_err(ApiError::from)?;
    let (bytes, content_type) = if let Some(attachment_id) = parse_attachment_blob(&blob_id) {
        let row = owned_attachment(
            &state.repos,
            ferroma_core::AttachmentId::new(attachment_id),
            auth.user_id(),
        )
        .await?;
        (state.attachments.read(&row.storage_path)?, row.content_type)
    } else if let Some(message_id) = parse_message_blob(&blob_id) {
        let (_message, _mailbox, bytes) = state
            .mail_service
            .raw_message(MessageId::new(message_id), auth.user_id())
            .await?;
        (bytes, "message/rfc822".to_string())
    } else {
        return Err(ApiError::new(FerromaError::NotFound(
            "no such blob".to_string(),
        )));
    };
    let mut response = (StatusCode::OK, Body::from(bytes)).into_response();
    let content_type = HeaderValue::from_str(&content_type)
        .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream"));
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, content_type);
    Ok(response)
}

async fn dispatch(
    state: &AppState,
    auth: &JmapAuth,
    method: &str,
    args: &Map<String, Value>,
) -> Result<(String, Value), (String, String)> {
    match method {
        "Mailbox/get" => mailbox_get(state, auth, args)
            .await
            .map(|value| ("Mailbox/get".to_string(), value)),
        "Email/query" => email_query(state, auth, args)
            .await
            .map(|value| ("Email/query".to_string(), value)),
        "Email/get" => email_get(state, auth, args)
            .await
            .map(|value| ("Email/get".to_string(), value)),
        "Email/set" => email_set(state, auth, args)
            .await
            .map(|value| ("Email/set".to_string(), value)),
        "Email/import" => email_import(state, auth, args)
            .await
            .map(|value| ("Email/import".to_string(), value)),
        _ => Err((
            "unknownMethod".to_string(),
            format!("unsupported JMAP method {method}"),
        )),
    }
}

async fn mailbox_get(
    state: &AppState,
    auth: &JmapAuth,
    args: &Map<String, Value>,
) -> Result<Value, (String, String)> {
    check_account(args, auth.user_id().get())?;
    let wanted = ids(args)?;
    let mut list = Vec::new();
    let addresses = state
        .repos
        .mailboxes
        .list_by_user(auth.user_id())
        .await
        .map_err(server)?;
    for address in addresses {
        for folder in state
            .repos
            .folders
            .list(address.mailbox_id())
            .await
            .map_err(server)?
        {
            if wanted
                .as_ref()
                .is_none_or(|ids| ids.contains(&folder.id.to_string()))
            {
                list.push(json!({"id": folder.id.to_string(), "name": folder.name, "parentId": folder.parent_id.map(|id| id.to_string()), "role": folder_role(folder.special_use.as_deref(), &folder.name), "sortOrder": 0, "totalEmails": folder.message_count, "unreadEmails": folder.unseen_count, "totalThreads": folder.message_count, "unreadThreads": folder.unseen_count, "myRights": {"mayReadItems":true,"mayAddItems":true,"mayRemoveItems":true,"maySetSeen":true,"maySetKeywords":true,"mayCreateChild":true,"mayRename":true,"mayDelete":true,"maySubmit":true}, "isSubscribed": folder.subscribed}));
            }
        }
    }
    let returned: BTreeSet<String> = list
        .iter()
        .filter_map(|value| value.get("id").and_then(Value::as_str).map(str::to_string))
        .collect();
    let not_found = wanted
        .unwrap_or_default()
        .into_iter()
        .filter(|id| !returned.contains(id))
        .collect::<Vec<_>>();
    Ok(
        json!({"accountId": account_id(auth.user_id().get()), "state": session_state(state, auth.user_id().get()).await, "list": list, "notFound": not_found}),
    )
}

async fn email_query(
    state: &AppState,
    auth: &JmapAuth,
    args: &Map<String, Value>,
) -> Result<Value, (String, String)> {
    check_account(args, auth.user_id().get())?;
    let filter = args.get("filter").and_then(Value::as_object);
    let folder_id = filter
        .and_then(|filter| filter.get("inMailbox"))
        .and_then(Value::as_str)
        .and_then(|value| value.parse::<i64>().ok())
        .map(MailboxId::new);
    let text = filter
        .and_then(|filter| filter.get("text"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let unread = filter
        .and_then(|filter| filter.get("hasKeyword"))
        .and_then(Value::as_str)
        .is_some_and(|value| value.eq_ignore_ascii_case("$seen"));
    let rows = state
        .repos
        .messages
        .search(MessageSearch {
            folder_id,
            mailbox_id: None,
            subject: None,
            sender: None,
            text,
            unread_only: unread,
            flagged_only: false,
            with_attachments_only: false,
            since: None,
            before: None,
            limit: MAX_GET as i64,
            offset: 0,
        })
        .await
        .map_err(server)?;
    let allowed = owned_mailbox_ids(state, auth.user_id().get())
        .await
        .map_err(server)?;
    let ids = rows
        .into_iter()
        .filter(|row| allowed.contains(&row.mailbox_id))
        .map(|row| row.id.to_string())
        .collect::<Vec<_>>();
    Ok(
        json!({"accountId": account_id(auth.user_id().get()), "queryState": session_state(state, auth.user_id().get()).await, "canCalculateChanges": false, "position": 0, "ids": ids, "total": ids.len()}),
    )
}

async fn email_get(
    state: &AppState,
    auth: &JmapAuth,
    args: &Map<String, Value>,
) -> Result<Value, (String, String)> {
    check_account(args, auth.user_id().get())?;
    let wanted = ids(args)?;
    let rows = all_owned_messages(state, auth.user_id().get())
        .await
        .map_err(server)?;
    let mut list = Vec::new();
    let mut found = BTreeSet::new();
    for row in rows {
        if wanted
            .as_ref()
            .is_some_and(|ids| !ids.contains(&row.id.to_string()))
        {
            continue;
        }
        found.insert(row.id.to_string());
        let recipients = state
            .repos
            .messages
            .recipients(row.message_id())
            .await
            .map_err(server)?;
        let attachments = state
            .repos
            .attachments
            .list_by_message(row.message_id())
            .await
            .map_err(server)?;
        let (from, to, cc, bcc, reply_to) = jmap_addresses(&row, &recipients);
        let keywords = keywords(&row.flags);
        list.push(json!({"id": row.id.to_string(), "blobId": format!("M{}", row.id), "threadId": row.thread_id.unwrap_or_else(|| row.id.to_string()), "mailboxIds": {row.folder_id.to_string(): true}, "keywords": keywords, "size": row.size_bytes.max(0), "receivedAt": row.received_at.to_rfc3339(), "sentAt": row.sent_at.map(|date| date.to_rfc3339()), "from": from, "to": to, "cc": cc, "bcc": bcc, "replyTo": reply_to, "subject": row.subject, "preview": row.snippet, "hasAttachment": row.has_attachments, "attachments": attachments.into_iter().map(|attachment| json!({"blobId": attachment_blob_id(attachment.id), "type": attachment.content_type, "name": attachment.filename, "size": attachment.size_bytes})).collect::<Vec<_>>() }));
    }
    let not_found = wanted
        .unwrap_or_default()
        .into_iter()
        .filter(|id| !found.contains(id))
        .collect::<Vec<_>>();
    Ok(
        json!({"accountId": account_id(auth.user_id().get()), "state": session_state(state, auth.user_id().get()).await, "list": list, "notFound": not_found}),
    )
}

async fn email_set(
    state: &AppState,
    auth: &JmapAuth,
    args: &Map<String, Value>,
) -> Result<Value, (String, String)> {
    check_account(args, auth.user_id().get())?;
    if args.get("create").is_some() {
        return Err((
            "invalidArguments".to_string(),
            "Email/set cannot create Email objects; use Email/import or submission".to_string(),
        ));
    }
    let mut updated = Map::new();
    let mut not_updated = Map::new();
    let mut destroyed = Vec::new();
    let mut not_destroyed = Map::new();
    if let Some(updates) = args.get("update").and_then(Value::as_object) {
        if updates.len() > MAX_SET {
            return Err((
                "invalidArguments".to_string(),
                "too many updates".to_string(),
            ));
        }
        for (id, patch) in updates {
            let Ok(id) = id.parse::<i64>() else {
                not_updated.insert(id.clone(), json!({"type":"notFound"}));
                continue;
            };
            let Some(patch) = patch.as_object() else {
                not_updated.insert(id.to_string(), json!({"type":"invalidProperties"}));
                continue;
            };
            let keywords = patch.get("keywords").and_then(Value::as_object);
            let mailbox_ids = patch.get("mailboxIds").and_then(Value::as_object);
            if mailbox_ids.is_some() {
                not_updated.insert(
                    id.to_string(),
                    json!({"type":"invalidProperties", "properties":["mailboxIds"]}),
                );
                continue;
            }
            let seen = keywords
                .and_then(|values| values.get("$seen"))
                .and_then(Value::as_bool);
            let flagged = keywords
                .and_then(|values| values.get("$flagged"))
                .and_then(Value::as_bool);
            match state
                .mail_service
                .set_message_flags(
                    MessageId::new(id),
                    auth.user_id(),
                    seen,
                    flagged,
                    None,
                    None,
                )
                .await
            {
                Ok(_) => {
                    updated.insert(id.to_string(), Value::Null);
                }
                Err(FerromaError::NotFound(_)) => {
                    not_updated.insert(id.to_string(), json!({"type":"notFound"}));
                }
                Err(error) => return Err(server(error)),
            }
        }
    }
    if let Some(ids) = args.get("destroy").and_then(Value::as_array) {
        for id in ids {
            let Some(raw) = id.as_str() else {
                continue;
            };
            match raw.parse::<i64>() {
                Ok(id) => match state
                    .mail_service
                    .delete_message(MessageId::new(id), auth.user_id(), true)
                    .await
                {
                    Ok(()) => destroyed.push(raw.to_string()),
                    Err(FerromaError::NotFound(_)) => {
                        not_destroyed.insert(raw.to_string(), json!({"type":"notFound"}));
                    }
                    Err(error) => return Err(server(error)),
                },
                Err(_) => {
                    not_destroyed.insert(raw.to_string(), json!({"type":"notFound"}));
                }
            }
        }
    }
    let new_state = session_state(state, auth.user_id().get()).await;
    Ok(
        json!({"accountId": account_id(auth.user_id().get()), "oldState": new_state, "newState": new_state, "updated": updated, "notUpdated": not_updated, "destroyed": destroyed, "notDestroyed": not_destroyed}),
    )
}

async fn email_import(
    state: &AppState,
    auth: &JmapAuth,
    args: &Map<String, Value>,
) -> Result<Value, (String, String)> {
    check_account(args, auth.user_id().get())?;
    let emails = args
        .get("emails")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            (
                "invalidArguments".to_string(),
                "emails must be an object".to_string(),
            )
        })?;
    if emails.len() > MAX_SET {
        return Err((
            "invalidArguments".to_string(),
            "too many emails".to_string(),
        ));
    }
    let mut created = Map::new();
    let mut not_created = Map::new();
    for (creation_id, import) in emails {
        let Some(import) = import.as_object() else {
            not_created.insert(creation_id.clone(), json!({"type":"invalidProperties"}));
            continue;
        };
        let Some(blob_id) = import.get("blobId").and_then(Value::as_str) else {
            not_created.insert(
                creation_id.clone(),
                json!({"type":"invalidProperties", "properties":["blobId"]}),
            );
            continue;
        };
        let Some(folder_id) = import
            .get("mailboxIds")
            .and_then(Value::as_object)
            .and_then(|mailboxes| {
                mailboxes.iter().find_map(|(id, member)| {
                    member
                        .as_bool()
                        .filter(|value| *value)
                        .and_then(|_| id.parse::<i64>().ok())
                })
            })
        else {
            not_created.insert(
                creation_id.clone(),
                json!({"type":"invalidProperties", "properties":["mailboxIds"]}),
            );
            continue;
        };
        let Some(attachment_id) = parse_attachment_blob(blob_id) else {
            not_created.insert(creation_id.clone(), json!({"type":"blobNotFound"}));
            continue;
        };
        let attachment = match owned_attachment(
            &state.repos,
            ferroma_core::AttachmentId::new(attachment_id),
            auth.user_id(),
        )
        .await
        {
            Ok(row) => row,
            Err(_) => {
                not_created.insert(creation_id.clone(), json!({"type":"blobNotFound"}));
                continue;
            }
        };
        let raw = match state.attachments.read(&attachment.storage_path) {
            Ok(bytes) => bytes,
            Err(_) => {
                not_created.insert(creation_id.clone(), json!({"type":"blobNotFound"}));
                continue;
            }
        };
        let folder = MailboxId::new(folder_id);
        let mailbox = match state
            .repos
            .folders
            .find_by_id(folder)
            .await
            .map_err(server)?
        {
            Some(folder) => MailboxId::new(folder.mailbox_id),
            None => {
                not_created.insert(creation_id.clone(), json!({"type":"notFound"}));
                continue;
            }
        };
        let received_at = import
            .get("receivedAt")
            .and_then(Value::as_str)
            .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
            .map(|value| value.with_timezone(&Utc));
        let flags = import
            .get("keywords")
            .and_then(Value::as_object)
            .map(keyword_flags)
            .unwrap_or_default();
        match state
            .mail_service
            .import_raw_message(auth.user_id(), mailbox, folder, &raw, &flags, received_at)
            .await
        {
            Ok(message) => {
                created.insert(creation_id.clone(), json!({"id": message.id.to_string()}));
            }
            Err(FerromaError::Invalid(_) | FerromaError::LimitExceeded(_)) => {
                not_created.insert(creation_id.clone(), json!({"type":"invalidEmail"}));
            }
            Err(error) => return Err(server(error)),
        }
    }
    let state_value = session_state(state, auth.user_id().get()).await;
    Ok(
        json!({"accountId": account_id(auth.user_id().get()), "oldState": state_value, "newState": state_value, "created": created, "notCreated": not_created}),
    )
}

fn invalid(message: &str) -> ApiError {
    ApiError::new(FerromaError::Invalid(message.to_string()))
}
fn server(error: impl std::fmt::Display) -> (String, String) {
    ("serverFail".to_string(), error.to_string())
}
fn account_id(user_id: i64) -> String {
    format!("u{user_id}")
}
fn attachment_blob_id(id: i64) -> String {
    format!("A{id}")
}
fn parse_attachment_blob(value: &str) -> Option<i64> {
    value.strip_prefix('A')?.parse().ok()
}
fn parse_message_blob(value: &str) -> Option<i64> {
    value.strip_prefix('M')?.parse().ok()
}
fn require_account(account: &str, user_id: i64) -> Result<(), FerromaError> {
    if account == account_id(user_id) {
        Ok(())
    } else {
        Err(FerromaError::NotFound("no such JMAP account".to_string()))
    }
}
fn check_account(args: &Map<String, Value>, user_id: i64) -> Result<(), (String, String)> {
    match args.get("accountId").and_then(Value::as_str) {
        None => Ok(()),
        Some(account) if account == account_id(user_id) => Ok(()),
        _ => Err((
            "accountNotFound".to_string(),
            "no such JMAP account".to_string(),
        )),
    }
}
fn ids(args: &Map<String, Value>) -> Result<Option<BTreeSet<String>>, (String, String)> {
    match args.get("ids") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(ids)) => Ok(Some(
            ids.iter()
                .map(|value| {
                    value.as_str().map(str::to_string).ok_or_else(|| {
                        (
                            "invalidArguments".to_string(),
                            "ids must be strings".to_string(),
                        )
                    })
                })
                .collect::<Result<_, _>>()?,
        )),
        _ => Err((
            "invalidArguments".to_string(),
            "ids must be an array or null".to_string(),
        )),
    }
}
async fn session_state(state: &AppState, user_id: i64) -> String {
    state
        .sync
        .latest_cursor(ferroma_core::UserId::new(user_id))
        .await
        .map(|cursor| cursor.0.to_string())
        .unwrap_or_else(|_| "0".to_string())
}
fn public_base(state: &AppState) -> String {
    let configured = state.config.api.public_url.trim_end_matches('/');
    if configured.starts_with("http://") || configured.starts_with("https://") {
        configured.to_string()
    } else {
        format!("https://{}", state.config.server.hostname)
    }
}
async fn primary_address(state: &AppState, user_id: i64) -> Result<String, ApiError> {
    let rows = state
        .repos
        .mailboxes
        .list_by_user(ferroma_core::UserId::new(user_id))
        .await?;
    let mailbox = rows
        .iter()
        .find(|row| row.is_primary)
        .or(rows.first())
        .ok_or_else(|| {
            ApiError::new(FerromaError::NotFound(
                "account has no mailboxes".to_string(),
            ))
        })?;
    let domain = domain_name(&state.repos, mailbox.domain_id).await?;
    Ok(mailbox.address(&domain))
}
async fn owned_mailbox_ids(state: &AppState, user_id: i64) -> Result<BTreeSet<i64>, FerromaError> {
    Ok(state
        .repos
        .mailboxes
        .list_by_user(ferroma_core::UserId::new(user_id))
        .await?
        .into_iter()
        .map(|mailbox| mailbox.id)
        .collect())
}
async fn all_owned_messages(
    state: &AppState,
    user_id: i64,
) -> Result<Vec<ferroma_storage::models::Message>, FerromaError> {
    let mut result = Vec::new();
    for mailbox in state
        .repos
        .mailboxes
        .list_by_user(ferroma_core::UserId::new(user_id))
        .await?
    {
        result.extend(
            state
                .repos
                .messages
                .list_by_mailbox(mailbox.mailbox_id(), MAX_GET as i64, 0)
                .await?,
        );
    }
    Ok(result)
}
fn folder_role(special_use: Option<&str>, name: &str) -> Option<&'static str> {
    match special_use {
        Some("\\Sent") => Some("sent"),
        Some("\\Drafts") => Some("drafts"),
        Some("\\Trash") => Some("trash"),
        Some("\\Junk") => Some("junk"),
        Some("\\Archive") => Some("archive"),
        Some("\\All") => Some("all"),
        _ if name.eq_ignore_ascii_case("inbox") => Some("inbox"),
        _ => None,
    }
}
fn keywords(flags: &str) -> Value {
    let mut values = Map::new();
    for flag in flags.split_whitespace() {
        let value = match flag.to_ascii_lowercase().as_str() {
            "seen" => "$seen".to_string(),
            "flagged" => "$flagged".to_string(),
            "answered" => "$answered".to_string(),
            "draft" => "$draft".to_string(),
            other => other.to_string(),
        };
        values.insert(value, Value::Bool(true));
    }
    Value::Object(values)
}
fn keyword_flags(keywords: &Map<String, Value>) -> String {
    keywords
        .iter()
        .filter_map(|(keyword, value)| {
            value
                .as_bool()
                .filter(|value| *value)
                .map(|_| match keyword.as_str() {
                    "$seen" => "seen".to_string(),
                    "$flagged" => "flagged".to_string(),
                    "$answered" => "answered".to_string(),
                    "$draft" => "draft".to_string(),
                    other => other.to_ascii_lowercase(),
                })
        })
        .collect::<Vec<_>>()
        .join(" ")
}
/// From, To, Cc, Bcc and Reply-To, in that order.
type JmapAddressLists = (Vec<Value>, Vec<Value>, Vec<Value>, Vec<Value>, Vec<Value>);

fn jmap_addresses(
    message: &ferroma_storage::models::Message,
    recipients: &[ferroma_storage::models::MessageRecipient],
) -> JmapAddressLists {
    let from = message
        .sender
        .as_ref()
        .map(|address| vec![json!({"email":address,"name":message.sender_name})])
        .unwrap_or_default();
    let address = |kind: &str| {
        recipients
            .iter()
            .filter(|recipient| recipient.kind == kind)
            .map(|recipient| json!({"email":recipient.address,"name":recipient.display_name}))
            .collect()
    };
    (
        from,
        address("to"),
        address("cc"),
        address("bcc"),
        address("reply-to"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_and_blob_ids_are_opaque_and_round_trip() {
        assert_eq!(account_id(42), "u42");
        assert_eq!(attachment_blob_id(91), "A91");
        assert_eq!(parse_attachment_blob("A91"), Some(91));
        assert_eq!(parse_message_blob("M91"), Some(91));
        assert_eq!(parse_attachment_blob("M91"), None);
        assert_eq!(parse_attachment_blob("Ainvalid"), None);
    }

    #[test]
    fn account_validation_never_accepts_another_user() {
        assert!(require_account("u7", 7).is_ok());
        assert!(require_account("u8", 7).is_err());
        assert!(require_account("7", 7).is_err());
    }

    #[test]
    fn standard_jmap_keywords_map_to_the_shared_flag_vocabulary() {
        let input = serde_json::from_value(json!({
            "$seen": true,
            "$flagged": true,
            "$answered": false,
            "project": true
        }))
        .expect("keyword map");
        assert_eq!(keyword_flags(&input), "flagged seen project");
        let rendered = keywords("seen flagged project");
        assert_eq!(rendered["$seen"], true);
        assert_eq!(rendered["$flagged"], true);
        assert_eq!(rendered["project"], true);
    }

    #[test]
    fn mailbox_roles_follow_standard_special_use_names() {
        assert_eq!(folder_role(Some("\\Sent"), "Sent"), Some("sent"));
        assert_eq!(folder_role(None, "INBOX"), Some("inbox"));
        assert_eq!(folder_role(None, "Projects"), None);
    }

    #[test]
    fn ids_accept_null_or_strings_only() {
        let none = Map::new();
        assert_eq!(ids(&none).expect("absent ids"), None);
        let mut requested = Map::new();
        requested.insert("ids".to_string(), json!(["3", "9"]));
        assert_eq!(
            ids(&requested).expect("string ids"),
            Some(BTreeSet::from(["3".to_string(), "9".to_string()]))
        );
        requested.insert("ids".to_string(), json!([3]));
        assert!(ids(&requested).is_err());
    }
}
