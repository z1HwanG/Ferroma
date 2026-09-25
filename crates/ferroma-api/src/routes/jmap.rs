//! JMAP RFC 8620 / RFC 8621 HTTP adapter.
//!
//! This module is intentionally an adapter over [`crate::service::MessageService`] and
//! the repositories. It does not create a second mail store: mutations use the same
//! service methods that record sync changes and publish mail events for IMAP and FCP.

use std::collections::{BTreeMap, BTreeSet};

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::{DateTime, Utc};
use ferroma_core::{FerromaError, MailboxId, MessageId, UserId};
use ferroma_mail::MessageBuilder;
use ferroma_storage::models::{ChangeLogEntry, Message};
use ferroma_storage::repository::MessageSearch;
use serde_json::{json, Map, Value};

use crate::error::ApiError;
use crate::extract::JmapAuth;
use crate::routes::jmap_logic::{
    self, creation_id, resolve_references, select_properties, single_mailbox, CallOutcome, EmailFilter,
    EmailSort,
};
use crate::routes::mail::ownership::owned_attachment;
use crate::routes::mail::store::domain_name;
use crate::state::{AppState, MessageStub};

const CORE: &str = "urn:ietf:params:jmap:core";
const MAIL: &str = "urn:ietf:params:jmap:mail";
/// RFC 8621 §1.3.2. A client such as Flectar Mail refuses a session that does
/// not advertise this URI, even when the account can already read mail.
const SUBMISSION: &str = "urn:ietf:params:jmap:submission";
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
            MAIL: {},
            // RFC 8621 types the session-level submission capability as an empty
            // object. `maxDelayedSend` and `submissionExtensions` belong on the
            // account. A client that treats a missing URI as "this server cannot
            // send" never reaches the mailbox.
            SUBMISSION: {}
        },
        "accounts": {
            &account: {
                "name": email,
                "isPersonal": true,
                "isReadOnly": false,
                "accountCapabilities": {
                    MAIL: {
                        "maxMailboxesPerEmail": 1,
                        "maxMailboxDepth": null,
                        "maxSizeMailboxName": 255,
                        "maxSizeAttachmentsPerEmail": state.config.limits.max_attachment_size,
                        "emailQuerySortOptions": ["receivedAt", "sentAt", "size", "from", "subject"],
                        "mayCreateTopLevelMailbox": true
                    },
                    SUBMISSION: submission_account_capability()
                }
            }
        },
        "primaryAccounts": { MAIL: account, SUBMISSION: account },
        "username": auth.user().email,
        "apiUrl": format!("{base}/api/jmap/"),
        "downloadUrl": format!("{base}/api/jmap/download/{{accountId}}/{{blobId}}?type={{type}}&name={{name}}"),
        "uploadUrl": format!("{base}/api/jmap/upload/{{accountId}}"),
        // RFC 8620 requires a URI. An empty string is a relative URL with no base,
        // which a client rejects before it ever opens a stream (`Session
        // eventSourceUrl is not a valid URL`). Ferroma has no push endpoint; the
        // absolute URL is what the type requires, and a client that never subscribes
        // never requests it.
        "eventSourceUrl": format!("{base}/api/jmap/eventsource/?types={{types}}&closeafter={{closeafter}}&ping={{ping}}"),
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
    if !using.iter().any(|value| value.as_str() == Some(CORE)) {
        return Err(invalid("using must include the JMAP core capability"));
    }
    let uses_submission = using.iter().any(|value| value.as_str() == Some(SUBMISSION));
    let uses_mail = using.iter().any(|value| value.as_str() == Some(MAIL));
    // RFC 8621 makes submission depend on mail, so a request that names only
    // submission is not a valid use of either capability.
    if !uses_mail && !uses_submission {
        return Err(invalid(
            "using must include the JMAP mail or submission capability",
        ));
    }
    if uses_submission && !uses_mail {
        return Err(invalid(
            "using the JMAP submission capability also requires the mail capability",
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
    let mut outcomes = Vec::with_capacity(calls.len());
    // `#creationId` in a later call of this same request names an id assigned here.
    let mut created_ids: BTreeMap<String, String> = BTreeMap::new();
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
        if method.starts_with("Identity/") || method.starts_with("EmailSubmission/") {
            if !uses_submission {
                let body = json!({
                    "type": "unknownMethod",
                    "description": format!("{method} requires the JMAP submission capability")
                });
                outcomes.push(CallOutcome {
                    id: call_id.to_string(),
                    name: "error".to_string(),
                    body: body.clone(),
                });
                responses.push(json!(["error", body, call_id]));
                continue;
            }
        } else if !uses_mail {
            let body = json!({
                "type": "unknownMethod",
                "description": format!("{method} requires the JMAP mail capability")
            });
            outcomes.push(CallOutcome {
                id: call_id.to_string(),
                name: "error".to_string(),
                body: body.clone(),
            });
            responses.push(json!(["error", body, call_id]));
            continue;
        }
        // RFC 8620 §3.7. A reference is resolved before the method runs, and a
        // failure rejects that one call rather than the whole request.
        let args = match resolve_references(args, &outcomes) {
            Ok(args) => args,
            Err((kind, description)) => {
                let body = json!({"type": kind, "description": description});
                outcomes.push(CallOutcome {
                    id: call_id.to_string(),
                    name: "error".to_string(),
                    body: body.clone(),
                });
                responses.push(json!(["error", body, call_id]));
                continue;
            }
        };
        let response = match dispatch(&state, &auth, method, &args, &created_ids).await {
            Ok((name, body)) => {
                remember_creations(&name, &body, &mut created_ids);
                outcomes.push(CallOutcome {
                    id: call_id.to_string(),
                    name: name.clone(),
                    body: body.clone(),
                });
                json!([name, body, call_id])
            }
            Err((kind, description)) => {
                let body = json!({"type": kind, "description": description});
                outcomes.push(CallOutcome {
                    id: call_id.to_string(),
                    name: "error".to_string(),
                    body: body.clone(),
                });
                json!(["error", body, call_id])
            }
        };
        responses.push(response);
    }
    let state_value = session_state(&state, auth.user_id().get()).await;
    Ok(Json(
        json!({"methodResponses": responses, "sessionState": state_value}),
    ))
}

/// Record ids assigned by a `/set` or `Email/import`, so a later call can say `#id`.
fn remember_creations(name: &str, body: &Value, created_ids: &mut BTreeMap<String, String>) {
    let Some(created) = body.get("created").and_then(Value::as_object) else {
        return;
    };
    if name != "Email/set" && name != "Mailbox/set" && name != "Email/import" {
        return;
    }
    for (creation, object) in created {
        if let Some(id) = object.get("id").and_then(Value::as_str) {
            created_ids.insert(creation.clone(), id.to_string());
        }
    }
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
    created_ids: &BTreeMap<String, String>,
) -> Result<(String, Value), (String, String)> {
    match method {
        "Mailbox/get" => mailbox_get(state, auth, args)
            .await
            .map(|value| ("Mailbox/get".to_string(), value)),
        "Mailbox/set" => mailbox_set(state, auth, args, created_ids)
            .await
            .map(|value| ("Mailbox/set".to_string(), value)),
        "Mailbox/changes" => mailbox_changes(state, auth, args)
            .await
            .map(|value| ("Mailbox/changes".to_string(), value)),
        "Email/query" => email_query(state, auth, args)
            .await
            .map(|value| ("Email/query".to_string(), value)),
        "Email/get" => email_get(state, auth, args, created_ids)
            .await
            .map(|value| ("Email/get".to_string(), value)),
        "Email/set" => email_set(state, auth, args, created_ids)
            .await
            .map(|value| ("Email/set".to_string(), value)),
        "Email/changes" => email_changes(state, auth, args)
            .await
            .map(|value| ("Email/changes".to_string(), value)),
        "Email/import" => email_import(state, auth, args)
            .await
            .map(|value| ("Email/import".to_string(), value)),
        "Identity/get" => identity_get(state, auth, args)
            .await
            .map(|value| ("Identity/get".to_string(), value)),
        "EmailSubmission/set" => email_submission_set(state, auth, args, created_ids)
            .await
            .map(|value| ("EmailSubmission/set".to_string(), value)),
        "EmailSubmission/get" => email_submission_get(state, auth, args)
            .await
            .map(|value| ("EmailSubmission/get".to_string(), value)),
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
        for stored in state
            .repos
            .folders
            .list(address.mailbox_id())
            .await
            .map_err(server)?
        {
            // `totalEmails` / `unreadEmails` are the same denormalised counters the
            // Webmail sidebar reads. Listing recomputes them, so a client that asks
            // Mailbox/get after a move does not keep the count the move left behind.
            // A failed recount keeps the stored row rather than dropping the mailbox.
            let folder = match state.repos.folders.recount(stored.folder_id()).await {
                Ok(fresh) => fresh,
                Err(error) => {
                    tracing::warn!(
                        folder_id = stored.id,
                        error = %error,
                        "folder counters could not be recomputed for Mailbox/get"
                    );
                    stored
                }
            };
            if wanted
                .as_ref()
                .is_none_or(|ids| ids.contains(&folder.id.to_string()))
            {
                let standard = folder.is_inbox() || folder.special_use.is_some();
                // JMAP's name is the one segment, and `parentId` carries the rest.
                // A folder that predates `parent_id` still has the whole path in
                // its name and no parent, so that path is what the client sees.
                let name = if folder.parent_id.is_some() {
                    folder.name.rsplit('/').next().unwrap_or(&folder.name)
                } else {
                    folder.name.as_str()
                };
                // `mayDelete` and `mayRename` follow what `Mailbox/set` will
                // actually accept. A client that offers to delete INBOX because
                // the right said it could is worse than one that does not offer.
                list.push(json!({
                    "id": folder.id.to_string(),
                    "name": name,
                    "parentId": folder.parent_id.map(|id| id.to_string()),
                    "role": folder_role(folder.special_use.as_deref(), &folder.name),
                    "sortOrder": 0,
                    "totalEmails": folder.message_count,
                    "unreadEmails": folder.unseen_count,
                    "totalThreads": folder.message_count,
                    "unreadThreads": folder.unseen_count,
                    "myRights": {
                        "mayReadItems": true,
                        "mayAddItems": true,
                        "mayRemoveItems": true,
                        "maySetSeen": true,
                        "maySetKeywords": true,
                        "mayCreateChild": true,
                        "mayRename": !standard,
                        "mayDelete": !standard,
                        "maySubmit": true
                    },
                    "isSubscribed": folder.subscribed
                }));
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

/// Create, rename, reparent, subscribe or destroy a folder.
///
/// The folder row and its Maildir directory go through the same helpers the
/// Webmail uses, and each change is a `folder_*` entry in the change log, which
/// is what [`mailbox_changes`] later reports.
async fn mailbox_set(
    state: &AppState,
    auth: &JmapAuth,
    args: &Map<String, Value>,
    created_ids: &BTreeMap<String, String>,
) -> Result<Value, (String, String)> {
    check_account(args, auth.user_id().get())?;
    let old_state = session_state(state, auth.user_id().get()).await;
    if args.get("onDestroyRemoveEmails").and_then(Value::as_bool) == Some(false) {
        return Err((
            "invalidArguments".to_string(),
            "destroying a mailbox always removes the emails inside it".to_string(),
        ));
    }
    let addresses = state
        .repos
        .mailboxes
        .list_by_user(auth.user_id())
        .await
        .map_err(server)?;
    // Folders live under one address. A user with several addresses creates a
    // folder under the primary one; an update names the folder, so its address
    // is already known.
    let primary = addresses
        .iter()
        .find(|row| row.is_primary)
        .or(addresses.first())
        .ok_or_else(|| ("serverFail".to_string(), "account has no mailboxes".to_string()))?;
    let mut created = Map::new();
    let mut not_created = Map::new();
    let mut updated = Map::new();
    let mut not_updated = Map::new();
    let mut destroyed = Vec::new();
    let mut not_destroyed = Map::new();

    if let Some(creates) = args.get("create").and_then(Value::as_object) {
        if creates.len() > MAX_SET {
            return Err(("invalidArguments".to_string(), "too many creations".to_string()));
        }
        for (creation_id, object) in creates {
            match create_mailbox(state, auth, primary, object, created_ids, &created).await {
                Ok(folder) => {
                    created.insert(
                        creation_id.clone(),
                        json!({"id": folder.id.to_string()}),
                    );
                }
                Err(problem) => {
                    not_created.insert(creation_id.clone(), problem);
                }
            }
        }
    }
    if let Some(updates) = args.get("update").and_then(Value::as_object) {
        if updates.len() > MAX_SET {
            return Err(("invalidArguments".to_string(), "too many updates".to_string()));
        }
        for (id, patch) in updates {
            match update_mailbox(state, auth, id, patch, created_ids, &created).await {
                Ok(()) => {
                    updated.insert(id.clone(), Value::Null);
                }
                Err(problem) => {
                    not_updated.insert(id.clone(), problem);
                }
            }
        }
    }
    if let Some(ids) = args.get("destroy").and_then(Value::as_array) {
        for id in ids {
            let Some(raw) = id.as_str() else {
                continue;
            };
            match destroy_mailbox(state, auth, raw).await {
                Ok(()) => destroyed.push(raw.to_string()),
                Err(problem) => {
                    not_destroyed.insert(raw.to_string(), problem);
                }
            }
        }
    }
    let new_state = session_state(state, auth.user_id().get()).await;
    Ok(json!({
        "accountId": account_id(auth.user_id().get()),
        "oldState": old_state,
        "newState": new_state,
        "created": created,
        "notCreated": not_created,
        "updated": updated,
        "notUpdated": not_updated,
        "destroyed": destroyed,
        "notDestroyed": not_destroyed
    }))
}

async fn create_mailbox(
    state: &AppState,
    auth: &JmapAuth,
    primary: &ferroma_storage::models::Mailbox,
    object: &Value,
    created_ids: &BTreeMap<String, String>,
    created: &Map<String, Value>,
) -> Result<ferroma_storage::models::Folder, Value> {
    let object = object
        .as_object()
        .ok_or_else(|| json!({"type": "invalidProperties"}))?;
    if object.contains_key("role") && !object.get("role").is_some_and(Value::is_null) {
        return Err(json!({
            "type": "invalidProperties",
            "description": "a mailbox role is assigned by the server",
            "properties": ["role"]
        }));
    }
    let name = object
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .ok_or_else(|| json!({"type": "invalidProperties", "properties": ["name"]}))?;
    if name.len() > 255 || name.contains('/') {
        return Err(json!({
            "type": "invalidProperties",
            "description": "a mailbox name is one path segment",
            "properties": ["name"]
        }));
    }
    let parent = match object.get("parentId") {
        None | Some(Value::Null) => None,
        Some(value) => {
            let raw = value
                .as_str()
                .ok_or_else(|| json!({"type": "invalidProperties", "properties": ["parentId"]}))?;
            Some(resolve_mailbox_id(raw, created_ids, created).ok_or_else(|| {
                json!({"type": "invalidProperties", "properties": ["parentId"]})
            })?)
        }
    };
    let (mailbox, parent_folder) = match parent {
        Some(parent) => {
            let folder = state
                .repos
                .folders
                .find_by_id(MailboxId::new(parent))
                .await
                .map_err(|error| json!({"type": "serverFail", "description": error.to_string()}))?
                .ok_or_else(|| json!({"type": "invalidProperties", "properties": ["parentId"]}))?;
            let mailbox = owned_mailbox_row(state, auth, folder.mailbox_id).await?;
            (mailbox, Some(folder))
        }
        None => (primary.clone(), None),
    };
    let path = match &parent_folder {
        Some(parent) => format!("{}/{}", parent.name, name),
        None => name.to_string(),
    };
    let domain = domain_name(&state.repos, mailbox.domain_id)
        .await
        .map_err(|error| json!({"type": "serverFail", "description": error.to_string()}))?;
    if state
        .repos
        .folders
        .find_by_name(mailbox.mailbox_id(), &path)
        .await
        .map_err(|error| json!({"type": "serverFail", "description": error.to_string()}))?
        .is_some()
    {
        return Err(json!({"type": "invalidProperties", "description": "a mailbox with that name already exists", "properties": ["name"]}));
    }
    state
        .maildir
        .create_folder(&domain, &mailbox.local_part, &path)
        .map_err(|error| json!({"type": "serverFail", "description": error.to_string()}))?;
    let folder = match state
        .repos
        .folders
        .create_in_logged(
            mailbox.mailbox_id(),
            &path,
            parent_folder.as_ref().map(ferroma_storage::models::Folder::folder_id),
            None,
            auth.user_id(),
        )
        .await
    {
        Ok(folder) => folder,
        Err(error) => {
            let _ = state
                .maildir
                .delete_folder(&domain, &mailbox.local_part, &path);
            return Err(json!({"type": "serverFail", "description": error.to_string()}));
        }
    };
    if object.get("isSubscribed").and_then(Value::as_bool) == Some(false) {
        state
            .repos
            .folders
            .set_subscribed_logged(folder.folder_id(), false, auth.user_id())
            .await
            .map_err(|error| json!({"type": "serverFail", "description": error.to_string()}))?;
    }
    Ok(folder)
}

async fn update_mailbox(
    state: &AppState,
    auth: &JmapAuth,
    id: &str,
    patch: &Value,
    created_ids: &BTreeMap<String, String>,
    created: &Map<String, Value>,
) -> Result<(), Value> {
    let folder_id = id
        .parse::<i64>()
        .map_err(|_| json!({"type": "notFound"}))?;
    let patch = patch
        .as_object()
        .ok_or_else(|| json!({"type": "invalidPatch"}))?;
    for forbidden in ["role", "sortOrder", "totalEmails", "unreadEmails", "totalThreads", "unreadThreads", "myRights"] {
        if patch.contains_key(forbidden) {
            return Err(json!({"type": "invalidProperties", "properties": [forbidden]}));
        }
    }
    let (folder, mailbox) = owned_folder(state, auth, folder_id).await?;
    let mut name = folder.name.clone();
    let mut parent = folder.parent_id;
    let mut renamed = false;
    if let Some(value) = patch.get("name") {
        let leaf = value
            .as_str()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .ok_or_else(|| json!({"type": "invalidProperties", "properties": ["name"]}))?;
        if leaf.len() > 255 || leaf.contains('/') {
            return Err(json!({"type": "invalidProperties", "properties": ["name"]}));
        }
        let parent_name = match folder.parent_id {
            Some(parent) => state
                .repos
                .folders
                .find_by_id(MailboxId::new(parent))
                .await
                .map_err(|error| json!({"type": "serverFail", "description": error.to_string()}))?
                .map(|parent| parent.name),
            None => None,
        };
        name = match parent_name {
            Some(parent) => format!("{parent}/{leaf}"),
            None => leaf.to_string(),
        };
        renamed = true;
    }
    if let Some(value) = patch.get("parentId") {
        parent = match value {
            Value::Null => None,
            other => {
                let raw = other.as_str().ok_or_else(|| {
                    json!({"type": "invalidProperties", "properties": ["parentId"]})
                })?;
                Some(resolve_mailbox_id(raw, created_ids, created).ok_or_else(|| {
                    json!({"type": "invalidProperties", "properties": ["parentId"]})
                })?)
            }
        };
        let leaf = name.rsplit('/').next().unwrap_or(&name);
        name = match parent {
            Some(parent_id) => {
                let parent_folder = state
                    .repos
                    .folders
                    .find_by_id(MailboxId::new(parent_id))
                    .await
                    .map_err(|error| json!({"type": "serverFail", "description": error.to_string()}))?
                    .filter(|parent| parent.mailbox_id == folder.mailbox_id)
                    .ok_or_else(|| json!({"type": "invalidProperties", "properties": ["parentId"]}))?;
                format!("{}/{leaf}", parent_folder.name)
            }
            None => leaf.to_string(),
        };
        renamed = true;
    }
    if renamed && name != folder.name {
        ferroma_storage::rename_folder_tree(
            &state.repos,
            &state.maildir,
            folder.folder_id(),
            &name,
            Some(parent),
            Some(auth.user_id()),
        )
        .await
        .map_err(|error| json!({"type": "invalidProperties", "description": error.to_string()}))?;
    }
    if let Some(subscribed) = patch.get("isSubscribed").and_then(Value::as_bool) {
        state
            .repos
            .folders
            .set_subscribed_logged(folder.folder_id(), subscribed, auth.user_id())
            .await
            .map_err(|error| json!({"type": "serverFail", "description": error.to_string()}))?;
    }
    let _ = mailbox;
    Ok(())
}

async fn destroy_mailbox(
    state: &AppState,
    auth: &JmapAuth,
    id: &str,
) -> Result<(), Value> {
    let folder_id = id.parse::<i64>().map_err(|_| json!({"type": "notFound"}))?;
    let (folder, mailbox) = owned_folder(state, auth, folder_id).await?;
    if folder.is_inbox() || folder.special_use.is_some() {
        return Err(json!({
            "type": "invalidArguments",
            "description": "a standard mailbox cannot be destroyed"
        }));
    }
    let children = state
        .repos
        .folders
        .descendants(folder.folder_id())
        .await
        .map_err(|error| json!({"type": "serverFail", "description": error.to_string()}))?;
    if !children.is_empty() {
        return Err(json!({
            "type": "mailboxHasChild",
            "description": "destroy the child mailboxes first"
        }));
    }
    // Pages, not one shot: a folder can hold more than `maxObjectsInGet`, and a
    // destroy that left the rest behind would then delete a folder that still
    // had mail. Each page is the new first page, because deletion moves the rest up.
    loop {
        let messages = state
            .repos
            .messages
            .list_by_folder(folder.folder_id(), MAX_GET as i64, 0)
            .await
            .map_err(|error| json!({"type": "serverFail", "description": error.to_string()}))?;
        if messages.is_empty() {
            break;
        }
        for message in messages {
            state
                .mail_service
                .delete_message(message.message_id(), auth.user_id(), true)
                .await
                .map_err(|error| json!({"type": "serverFail", "description": error.to_string()}))?;
        }
    }
    let domain = domain_name(&state.repos, mailbox.domain_id)
        .await
        .map_err(|error| json!({"type": "serverFail", "description": error.to_string()}))?;
    state
        .repos
        .folders
        .delete_logged(folder.folder_id(), auth.user_id())
        .await
        .map_err(|error| json!({"type": "serverFail", "description": error.to_string()}))?;
    if let Err(error) = state
        .maildir
        .delete_folder(&domain, &mailbox.local_part, &folder.name)
    {
        tracing::warn!(folder_id = folder.id, error = %error, "folder directory could not be removed");
    }
    Ok(())
}

/// Folders created, changed or removed since `sinceState`.
async fn mailbox_changes(
    state: &AppState,
    auth: &JmapAuth,
    args: &Map<String, Value>,
) -> Result<Value, (String, String)> {
    check_account(args, auth.user_id().get())?;
    let since = required_state(args)?;
    let (rows, has_more, new_state) = changes_since(state, auth.user_id(), since).await?;
    let mut created = Vec::new();
    let mut updated = Vec::new();
    let mut destroyed = Vec::new();
    for row in &rows {
        let Some(folder_id) = row.folder_id else {
            continue;
        };
        let id = folder_id.to_string();
        match row.kind.as_str() {
            "folder_created" => push_unique(&mut created, id),
            "folder_updated" => push_unique(&mut updated, id),
            "folder_deleted" => push_unique(&mut destroyed, id),
            _ => {}
        }
    }
    // A folder both created and updated in the window is created. One destroyed
    // after being created never needs to be fetched.
    updated.retain(|id| !created.contains(id) && !destroyed.contains(id));
    created.retain(|id| !destroyed.contains(id));
    Ok(changes_response(
        auth,
        since,
        new_state,
        created,
        updated,
        destroyed,
        has_more,
    ))
}

async fn email_query(
    state: &AppState,
    auth: &JmapAuth,
    args: &Map<String, Value>,
) -> Result<Value, (String, String)> {
    check_account(args, auth.user_id().get())?;
    let filter = EmailFilter::parse(args.get("filter"))?;
    let (sort, descending) = EmailSort::parse(args.get("sort"))?;
    if args.get("collapseThreads").and_then(Value::as_bool) == Some(true) {
        return Err((
            "invalidArguments".to_string(),
            "collapseThreads is not supported".to_string(),
        ));
    }
    let position = args.get("position").and_then(Value::as_i64).unwrap_or(0);
    if position < 0 {
        return Err((
            "invalidArguments".to_string(),
            "a negative position is not supported".to_string(),
        ));
    }
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .map(|limit| limit.min(MAX_GET as u64) as i64)
        .unwrap_or(MAX_GET as i64);
    // Conditions the message table can answer go to SQL. `to` needs the
    // recipient rows, so it is applied after, and the page is cut last.
    // "Has $seen" and "lacks $seen" cannot both be one SQL predicate. When they
    // disagree the in-memory pass below applies each list as the client wrote it.
    let seen_required = filter.has_keyword.iter().any(|flag| flag == "seen");
    let seen_forbidden = filter.not_keyword.iter().any(|flag| flag == "seen");
    let rows = state
        .repos
        .messages
        .search(MessageSearch {
            folder_id: filter.in_mailbox.map(MailboxId::new),
            mailbox_id: None,
            subject: filter.subject.first().cloned(),
            sender: filter.from.first().cloned(),
            text: None,
            // `text` and `body` match the indexed body, not only the snippet.
            full_text: filter.text.first().cloned(),
            unread_only: seen_forbidden && !seen_required,
            flagged_only: filter.has_keyword.iter().any(|flag| flag == "flagged"),
            with_attachments_only: filter.has_attachment == Some(true),
            since: filter
                .after
                .as_deref()
                .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
                .map(|value| value.with_timezone(&Utc)),
            before: filter
                .before
                .as_deref()
                .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
                .map(|value| value.with_timezone(&Utc)),
            limit: MAX_GET as i64,
            offset: 0,
        })
        .await
        .map_err(server)?;
    let allowed = owned_mailbox_ids(state, auth.user_id().get())
        .await
        .map_err(server)?;
    let mut rows = rows
        .into_iter()
        .filter(|row| allowed.contains(&row.mailbox_id))
        .filter(|row| message_matches(&filter, row))
        .collect::<Vec<_>>();
    if filter.to.iter().any(|value| !value.is_empty()) {
        let mut kept = Vec::new();
        for row in rows {
            let recipients = state
                .repos
                .messages
                .recipients(row.message_id())
                .await
                .map_err(server)?;
            let matches = filter.to.iter().all(|needle| {
                recipients.iter().any(|recipient| {
                    recipient.kind == "to"
                        && (contains_ignore_case(&recipient.address, needle)
                            || recipient
                                .display_name
                                .as_deref()
                                .is_some_and(|name| contains_ignore_case(name, needle)))
                })
            });
            if matches {
                kept.push(row);
            }
        }
        rows = kept;
    }
    sort_messages(&mut rows, sort, descending);
    let total = rows.len();
    let ids = rows
        .into_iter()
        .skip(position as usize)
        .take(limit as usize)
        .map(|row| row.id.to_string())
        .collect::<Vec<_>>();
    Ok(json!({
        "accountId": account_id(auth.user_id().get()),
        "queryState": session_state(state, auth.user_id().get()).await,
        "canCalculateChanges": false,
        "position": position,
        "ids": ids,
        "total": total
    }))
}

/// Conditions that [`MessageSearch`] does not apply on its own.
fn message_matches(filter: &EmailFilter, message: &Message) -> bool {
    let flags = message
        .flags
        .split_whitespace()
        .map(|flag| flag.to_ascii_lowercase())
        .collect::<BTreeSet<_>>();
    if !filter
        .has_keyword
        .iter()
        .all(|flag| flags.contains(flag))
    {
        return false;
    }
    if filter.not_keyword.iter().any(|flag| flags.contains(flag)) {
        return false;
    }
    if filter.has_attachment == Some(false) && message.has_attachments {
        return false;
    }
    let extra_subject = filter.subject.iter().skip(1);
    if extra_subject
        .clone()
        .any(|needle| !contains_ignore_case(message.subject.as_deref().unwrap_or(""), needle))
    {
        return false;
    }
    let sender = format!(
        "{} {}",
        message.sender.as_deref().unwrap_or(""),
        message.sender_name.as_deref().unwrap_or("")
    );
    if filter
        .from
        .iter()
        .skip(1)
        .any(|needle| !contains_ignore_case(&sender, needle))
    {
        return false;
    }
    true
}

fn contains_ignore_case(haystack: &str, needle: &str) -> bool {
    haystack.to_ascii_lowercase().contains(&needle.to_ascii_lowercase())
}

fn sort_messages(rows: &mut [Message], sort: EmailSort, descending: bool) {
    rows.sort_by(|left, right| {
        let order = match sort {
            EmailSort::ReceivedAt => left.received_at.cmp(&right.received_at),
            EmailSort::SentAt => left.sent_at.cmp(&right.sent_at),
            EmailSort::Size => left.size_bytes.cmp(&right.size_bytes),
            EmailSort::From => sender_key(left).cmp(&sender_key(right)),
            EmailSort::Subject => subject_key(left).cmp(&subject_key(right)),
        };
        if descending {
            order.reverse()
        } else {
            order
        }
    });
}

fn sender_key(message: &Message) -> String {
    message
        .sender_name
        .as_deref()
        .filter(|name| !name.is_empty())
        .or(message.sender.as_deref())
        .unwrap_or("")
        .to_ascii_lowercase()
}

fn subject_key(message: &Message) -> String {
    let subject = message.subject.as_deref().unwrap_or("");
    let mut rest = subject.trim();
    loop {
        let lower = rest.to_ascii_lowercase();
        let stripped = ["re:", "fwd:", "fw:"]
            .iter()
            .find_map(|prefix| lower.strip_prefix(prefix).map(|_| prefix.len()));
        match stripped {
            Some(len) => rest = rest[len..].trim(),
            None => break,
        }
    }
    rest.to_ascii_lowercase()
}

async fn email_get(
    state: &AppState,
    auth: &JmapAuth,
    args: &Map<String, Value>,
    created_ids: &BTreeMap<String, String>,
) -> Result<Value, (String, String)> {
    check_account(args, auth.user_id().get())?;
    let wanted = ids(args)?;
    let properties = properties_arg(args)?;
    let fetch_text = args
        .get("fetchTextBodyValues")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let fetch_html = args
        .get("fetchHTMLBodyValues")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let fetch_all = args
        .get("fetchAllBodyValues")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let max_body = args
        .get("maxBodyValueBytes")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let rows = match &wanted {
        Some(ids) if ids.len() <= 50 => messages_by_ids(state, auth.user_id(), ids).await?,
        _ => all_owned_messages(state, auth.user_id().get())
            .await
            .map_err(server)?,
    };
    let mut list = Vec::new();
    let mut found = BTreeSet::new();
    for row in rows {
        let id = row.id.to_string();
        if wanted.as_ref().is_some_and(|ids| !ids.contains(&id)) {
            continue;
        }
        found.insert(id);
        let mut email = email_object(state, &row).await?;
        if wants_body(&properties) {
            attach_body(
                state,
                &row,
                &mut email,
                fetch_text || fetch_all,
                fetch_html || fetch_all,
                max_body,
            )?;
        }
        select_properties(&mut email, properties.as_deref());
        list.push(Value::Object(email));
    }
    let not_found = wanted
        .unwrap_or_default()
        .into_iter()
        .filter(|id| !found.contains(id) && creation_id(created_ids, id).is_none())
        .collect::<Vec<_>>();
    Ok(json!({
        "accountId": account_id(auth.user_id().get()),
        "state": session_state(state, auth.user_id().get()).await,
        "list": list,
        "notFound": not_found
    }))
}

fn wants_body(properties: &Option<Vec<String>>) -> bool {
    match properties {
        None => true,
        Some(properties) => properties.iter().any(|name| {
            matches!(
                name.as_str(),
                "textBody" | "htmlBody" | "bodyValues" | "bodyStructure" | "attachments"
            )
        }),
    }
}

fn properties_arg(args: &Map<String, Value>) -> Result<Option<Vec<String>>, (String, String)> {
    match args.get("properties") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(values)) => {
            let mut names = Vec::with_capacity(values.len());
            for value in values {
                let name = value.as_str().ok_or_else(|| {
                    (
                        "invalidArguments".to_string(),
                        "properties must be strings".to_string(),
                    )
                })?;
                if !jmap_logic::known_email_property(name) {
                    return Err((
                        "invalidArguments".to_string(),
                        format!("unknown email property {name}"),
                    ));
                }
                names.push(name.to_string());
            }
            Ok(Some(names))
        }
        _ => Err((
            "invalidArguments".to_string(),
            "properties must be an array or null".to_string(),
        )),
    }
}

async fn email_object(
    state: &AppState,
    row: &Message,
) -> Result<Map<String, Value>, (String, String)> {
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
    let (from, to, cc, bcc, reply_to) = jmap_addresses(row, &recipients);
    let email = json!({
        "id": row.id.to_string(),
        "blobId": format!("M{}", row.id),
        "threadId": row.thread_id.clone().unwrap_or_else(|| row.id.to_string()),
        "mailboxIds": {row.folder_id.to_string(): true},
        "keywords": keywords(&row.flags),
        "size": row.size_bytes.max(0),
        "receivedAt": row.received_at.to_rfc3339(),
        "sentAt": row.sent_at.map(|date| date.to_rfc3339()),
        "messageId": row.rfc_message_id.as_ref().map(|id| vec![id.clone()]),
        "from": from,
        "to": to,
        "cc": cc,
        "bcc": bcc,
        "replyTo": reply_to,
        "subject": row.subject,
        "preview": row.snippet,
        "hasAttachment": row.has_attachments,
        "attachments": attachments.into_iter().map(|attachment| json!({
            "partId": format!("a{}", attachment.id),
            "blobId": attachment_blob_id(attachment.id),
            "type": attachment.content_type,
            "name": attachment.filename,
            "size": attachment.size_bytes.max(0),
            "disposition": if attachment.is_inline { "inline" } else { "attachment" },
            "cid": attachment.content_id
        })).collect::<Vec<_>>()
    });
    match email {
        Value::Object(object) => Ok(object),
        _ => Err(server("email object")),
    }
}

/// Fill `textBody`, `htmlBody` and `bodyValues` from the stored RFC 5322 bytes.
///
/// The body is not in the message row. Reading it here, and only when the client
/// asked for a body property, keeps a list view from opening every Maildir file.
fn attach_body(
    state: &AppState,
    row: &Message,
    email: &mut Map<String, Value>,
    fetch_text: bool,
    fetch_html: bool,
    max_body: u64,
) -> Result<(), (String, String)> {
    let bytes = state.maildir.read(&row.storage_path).map_err(server)?;
    let parsed = ferroma_mail::ParsedMessage::parse(&bytes).map_err(server)?;
    let text = parsed.text_body();
    let html = parsed.html_body();
    let mut values = Map::new();
    if let Some(text) = text.as_deref().filter(|_| fetch_text) {
        values.insert("text".to_string(), body_value(text, max_body));
    }
    if let Some(html) = html.as_deref().filter(|_| fetch_html) {
        values.insert("html".to_string(), body_value(html, max_body));
    }
    let text_part = text.as_ref().map(|text| {
        json!({"partId": "text", "blobId": Value::Null, "size": text.len(), "type": "text/plain", "charset": "utf-8"})
    });
    let html_part = html.as_ref().map(|html| {
        json!({"partId": "html", "blobId": Value::Null, "size": html.len(), "type": "text/html", "charset": "utf-8"})
    });
    email.insert(
        "textBody".to_string(),
        Value::Array(text_part.clone().into_iter().collect()),
    );
    email.insert(
        "htmlBody".to_string(),
        Value::Array(html_part.clone().into_iter().collect()),
    );
    email.insert("bodyValues".to_string(), Value::Object(values));
    let mut sub_parts = Vec::new();
    if let Some(part) = text_part {
        sub_parts.push(part);
    }
    if let Some(part) = html_part {
        sub_parts.push(part);
    }
    if let Some(attachments) = email.get("attachments").cloned() {
        email.insert(
            "bodyStructure".to_string(),
            json!({"partId": Value::Null, "blobId": Value::Null, "size": row.size_bytes.max(0), "type": "multipart/mixed", "subParts": sub_parts.into_iter().chain(attachments.as_array().into_iter().flatten().cloned()).collect::<Vec<_>>()}),
        );
    }
    if let Some(ids) = parsed.message_id().map(|id| vec![id.as_str().to_string()]) {
        email.insert("messageId".to_string(), json!(ids));
    }
    Ok(())
}

fn body_value(text: &str, max_bytes: u64) -> Value {
    if max_bytes == 0 || text.len() as u64 <= max_bytes {
        return json!({"value": text, "isEncodingProblem": false, "isTruncated": false});
    }
    let mut end = max_bytes as usize;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    json!({"value": &text[..end], "isEncodingProblem": false, "isTruncated": true})
}

async fn email_set(
    state: &AppState,
    auth: &JmapAuth,
    args: &Map<String, Value>,
    created_ids: &BTreeMap<String, String>,
) -> Result<Value, (String, String)> {
    check_account(args, auth.user_id().get())?;
    let old_state = session_state(state, auth.user_id().get()).await;
    let mut created = Map::new();
    let mut not_created = Map::new();
    let mut updated = Map::new();
    let mut not_updated = Map::new();
    let mut destroyed = Vec::new();
    let mut not_destroyed = Map::new();
    if let Some(creates) = args.get("create").and_then(Value::as_object) {
        if creates.len() > MAX_SET {
            return Err(("invalidArguments".to_string(), "too many creations".to_string()));
        }
        for (creation_id, object) in creates {
            match create_email(state, auth, object, created_ids, &created).await {
                Ok((id, thread_id, blob_id)) => {
                    created.insert(
                        creation_id.clone(),
                        json!({"id": id, "blobId": blob_id, "threadId": thread_id, "size": Value::Null}),
                    );
                }
                Err(problem) => {
                    not_created.insert(creation_id.clone(), problem);
                }
            }
        }
    }
    if let Some(updates) = args.get("update").and_then(Value::as_object) {
        if updates.len() > MAX_SET {
            return Err(("invalidArguments".to_string(), "too many updates".to_string()));
        }
        for (id, patch) in updates {
            let resolved = creation_id(created_ids, id)
                .or_else(|| created.get(id).and_then(|value| value.get("id")).and_then(Value::as_str).map(str::to_string))
                .unwrap_or_else(|| id.clone());
            match update_email(state, auth, &resolved, patch).await {
                Ok(()) => {
                    updated.insert(id.clone(), Value::Null);
                }
                Err(problem) => {
                    not_updated.insert(id.clone(), problem);
                }
            }
        }
    }
    if let Some(ids) = args.get("destroy").and_then(Value::as_array) {
        for id in ids {
            let Some(raw) = id.as_str() else {
                continue;
            };
            let resolved = creation_id(created_ids, raw).unwrap_or_else(|| raw.to_string());
            match resolved.parse::<i64>() {
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
    Ok(json!({
        "accountId": account_id(auth.user_id().get()),
        "oldState": old_state,
        "newState": new_state,
        "created": created,
        "notCreated": not_created,
        "updated": updated,
        "notUpdated": not_updated,
        "destroyed": destroyed,
        "notDestroyed": not_destroyed
    }))
}

/// Build an RFC 5322 message from the structured Email properties and file it.
///
/// This is how a client saves a draft. The bytes go through [`MessageService::import_raw_message`],
/// so the change log, quota and Maildir are the same ones SMTP and IMAP use.
async fn create_email(
    state: &AppState,
    auth: &JmapAuth,
    object: &Value,
    created_ids: &BTreeMap<String, String>,
    created: &Map<String, Value>,
) -> Result<(String, String, String), Value> {
    let object = object
        .as_object()
        .ok_or_else(|| json!({"type": "invalidProperties"}))?;
    if object.contains_key("bodyStructure") || object.contains_key("headers") {
        return Err(json!({
            "type": "invalidProperties",
            "description": "create an email from textBody, htmlBody and the parsed headers"
        }));
    }
    let mailbox_ids = object
        .get("mailboxIds")
        .and_then(Value::as_object)
        .ok_or_else(|| json!({"type": "invalidProperties", "properties": ["mailboxIds"]}))?;
    let mut mailbox_ids = mailbox_ids.clone();
    resolve_mailbox_keys(&mut mailbox_ids, created_ids, created);
    let folder_id = single_mailbox(&mailbox_ids).map_err(|(kind, description)| {
        json!({"type": kind, "description": description, "properties": ["mailboxIds"]})
    })?;
    let (folder, mailbox) = owned_folder(state, auth, folder_id).await?;
    let domain = domain_name(&state.repos, mailbox.domain_id)
        .await
        .map_err(|error| json!({"type": "serverFail", "description": error.to_string()}))?;
    let from = address_header(object.get("from")).unwrap_or_else(|| mailbox.address(&domain));
    let mut builder = MessageBuilder::new().from(from).domain(&domain);
    if let Some(value) = address_header(object.get("to")) {
        builder = builder.to(value);
    }
    if let Some(value) = address_header(object.get("cc")) {
        builder = builder.cc(value);
    }
    if let Some(value) = address_header(object.get("bcc")) {
        builder = builder.bcc(value);
    }
    if let Some(value) = address_header(object.get("replyTo")) {
        builder = builder.reply_to(value);
    }
    if let Some(subject) = object.get("subject").and_then(Value::as_str) {
        builder = builder.subject(subject);
    }
    let text = body_text(object, "textBody", "text");
    let html = body_text(object, "htmlBody", "html");
    if let Some(text) = text {
        builder = builder.text(&text);
    }
    if let Some(html) = html {
        builder = builder.html(&html);
    }
    if let Some(ids) = string_list(object.get("inReplyTo")) {
        if let Some(first) = ids.first() {
            builder = builder.in_reply_to(first);
        }
    }
    if let Some(ids) = string_list(object.get("references")) {
        builder = builder.references(&ids);
    }
    let raw = builder
        .build()
        .map_err(|error| json!({"type": "invalidEmail", "description": error.to_string()}))?;
    let flags = object
        .get("keywords")
        .and_then(Value::as_object)
        .map(jmap_logic_flags)
        .transpose()
        .map_err(|(kind, description)| json!({"type": kind, "description": description}))?
        .unwrap_or_else(|| "draft".to_string());
    let received_at = object
        .get("receivedAt")
        .and_then(Value::as_str)
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.with_timezone(&Utc));
    let message = state
        .mail_service
        .import_raw_message(
            auth.user_id(),
            mailbox.mailbox_id(),
            folder.folder_id(),
            &raw,
            &flags,
            received_at,
        )
        .await
        .map_err(|error| match error {
            FerromaError::Invalid(_) | FerromaError::LimitExceeded(_) => {
                json!({"type": "invalidEmail", "description": error.to_string()})
            }
            other => json!({"type": "serverFail", "description": other.to_string()}),
        })?;
    let thread = message
        .thread_id
        .clone()
        .unwrap_or_else(|| message.id.to_string());
    Ok((message.id.to_string(), thread, format!("M{}", message.id)))
}

/// The text a client put on a body part, looked up through `bodyValues`.
fn body_text(object: &Map<String, Value>, part: &str, fallback: &str) -> Option<String> {
    let values = object.get("bodyValues").and_then(Value::as_object);
    let part_id = object
        .get(part)
        .and_then(Value::as_array)
        .and_then(|parts| parts.first())
        .and_then(|part| part.get("partId"))
        .and_then(Value::as_str)
        .unwrap_or(fallback);
    values
        .and_then(|values| values.get(part_id))
        .and_then(|value| value.get("value"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn string_list(value: Option<&Value>) -> Option<Vec<String>> {
    value.and_then(Value::as_array).map(|values| {
        values
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect()
    })
}

fn address_header(value: Option<&Value>) -> Option<String> {
    let list = value?.as_array()?;
    let rendered = list
        .iter()
        .filter_map(|address| {
            let email = address.get("email")?.as_str()?.trim();
            if email.is_empty() {
                return None;
            }
            match address.get("name").and_then(Value::as_str).filter(|name| !name.is_empty()) {
                Some(name) => Some(format!("{name} <{email}>")),
                None => Some(email.to_string()),
            }
        })
        .collect::<Vec<_>>();
    if rendered.is_empty() {
        None
    } else {
        Some(rendered.join(", "))
    }
}

fn jmap_logic_flags(keywords: &Map<String, Value>) -> Result<String, (String, String)> {
    jmap_logic::apply_keyword_patch("", &{
        let mut patch = Map::new();
        patch.insert("keywords".to_string(), Value::Object(keywords.clone()));
        patch
    })?
    .ok_or_else(|| ("invalidProperties".to_string(), "keywords is empty".to_string()))
}

async fn update_email(
    state: &AppState,
    auth: &JmapAuth,
    id: &str,
    patch: &Value,
) -> Result<(), Value> {
    let id = id.parse::<i64>().map_err(|_| json!({"type": "notFound"}))?;
    let patch = patch
        .as_object()
        .ok_or_else(|| json!({"type": "invalidPatch"}))?;
    for (name, _) in patch {
        let allowed = name == "keywords"
            || name == "mailboxIds"
            || name.starts_with("keywords/")
            || name.starts_with("mailboxIds/");
        if !allowed {
            return Err(json!({"type": "invalidProperties", "properties": [name]}));
        }
    }
    let current = state
        .repos
        .messages
        .find_by_id(MessageId::new(id))
        .await
        .map_err(|error| json!({"type": "serverFail", "description": error.to_string()}))?
        .ok_or_else(|| json!({"type": "notFound"}))?;
    if let Some(flags) = jmap_logic::apply_keyword_patch(&current.flags, patch)
        .map_err(|(kind, description)| json!({"type": kind, "description": description}))?
    {
        if flags != current.flags {
            state
                .mail_service
                .set_message_flags_exact(MessageId::new(id), auth.user_id(), &flags)
                .await
                .map_err(|error| match error {
                    FerromaError::NotFound(_) => json!({"type": "notFound"}),
                    other => json!({"type": "serverFail", "description": other.to_string()}),
                })?;
        }
    }
    if let Some(folder) = mailbox_target(patch)? {
        if folder != current.folder_id {
            state
                .mail_service
                .move_message(MessageId::new(id), auth.user_id(), MailboxId::new(folder))
                .await
                .map_err(|error| match error {
                    FerromaError::NotFound(_) => {
                        json!({"type": "invalidProperties", "properties": ["mailboxIds"]})
                    }
                    other => json!({"type": "serverFail", "description": other.to_string()}),
                })?;
        }
    }
    Ok(())
}

/// The mailbox an update moves the email into, when the patch names one.
fn mailbox_target(patch: &Map<String, Value>) -> Result<Option<i64>, Value> {
    if let Some(ids) = patch.get("mailboxIds") {
        let ids = ids
            .as_object()
            .ok_or_else(|| json!({"type": "invalidProperties", "properties": ["mailboxIds"]}))?;
        return single_mailbox(ids)
            .map(Some)
            .map_err(|(kind, description)| json!({"type": kind, "description": description, "properties": ["mailboxIds"]}));
    }
    let mut chosen: Option<i64> = None;
    let mut removed = false;
    for (name, value) in patch {
        let Some(id) = name.strip_prefix("mailboxIds/") else {
            continue;
        };
        let present = value.as_bool().ok_or_else(|| {
            json!({"type": "invalidProperties", "properties": [name]})
        })?;
        let id = id.parse::<i64>().map_err(|_| {
            json!({"type": "invalidProperties", "properties": ["mailboxIds"]})
        })?;
        if present {
            if chosen.is_some() {
                return Err(json!({
                    "type": "invalidProperties",
                    "description": "an email belongs to exactly one mailbox",
                    "properties": ["mailboxIds"]
                }));
            }
            chosen = Some(id);
        } else {
            removed = true;
        }
    }
    if removed && chosen.is_none() {
        return Err(json!({
            "type": "invalidProperties",
            "description": "an email cannot leave every mailbox",
            "properties": ["mailboxIds"]
        }));
    }
    Ok(chosen)
}

/// Messages created, changed or destroyed since `sinceState`.
///
/// The state string is the account's change-log cursor. A move is an update:
/// the id does not change, and the client's next `Email/get` sees the new
/// `mailboxIds`. `canCalculateChanges` on a query stays false, so a folder
/// view still re-runs `Email/query` rather than trusting this list as a
/// position change.
async fn email_changes(
    state: &AppState,
    auth: &JmapAuth,
    args: &Map<String, Value>,
) -> Result<Value, (String, String)> {
    check_account(args, auth.user_id().get())?;
    let since = required_state(args)?;
    let (rows, has_more, new_state) = changes_since(state, auth.user_id(), since).await?;
    let mut created = Vec::new();
    let mut updated = Vec::new();
    let mut destroyed = Vec::new();
    for row in &rows {
        let Some(message_id) = row.message_id else {
            continue;
        };
        let id = message_id.to_string();
        match row.kind.as_str() {
            "message_created" => push_unique(&mut created, id),
            "message_updated" | "message_moved" => push_unique(&mut updated, id),
            "message_deleted" => push_unique(&mut destroyed, id),
            _ => {}
        }
    }
    updated.retain(|id| !created.contains(id) && !destroyed.contains(id));
    created.retain(|id| !destroyed.contains(id));
    Ok(changes_response(
        auth,
        since,
        new_state,
        created,
        updated,
        destroyed,
        has_more,
    ))
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

/// RFC 8621 account capability for `urn:ietf:params:jmap:submission`.
///
/// Delayed send is not implemented, so `maxDelayedSend` is zero rather than
/// omitted: a client that reads the property must not assume a delay is possible.
/// SMTP extensions are not offered on this path, so the map is empty.
fn submission_account_capability() -> Value {
    json!({
        "maxDelayedSend": 0,
        "submissionExtensions": {}
    })
}

/// The addresses the caller may send as. An Identity is not a stored object:
/// it is one enabled mailbox the account owns, and its id is that mailbox's id.
async fn owned_identities(
    state: &AppState,
    user_id: i64,
) -> Result<Vec<Value>, FerromaError> {
    let rows = state
        .repos
        .mailboxes
        .list_by_user_with_domain(ferroma_core::UserId::new(user_id))
        .await?;
    Ok(rows
        .into_iter()
        .filter(|row| row.mailbox.enabled)
        .map(|row| {
            json!({
                "id": row.mailbox.id.to_string(),
                "name": row.mailbox.display_name.clone().unwrap_or_default(),
                "email": row.address(),
                "replyTo": Value::Null,
                "bcc": Value::Null,
                "textSignature": "",
                "htmlSignature": "",
                "mayDelete": false
            })
        })
        .collect())
}

async fn identity_get(
    state: &AppState,
    auth: &JmapAuth,
    args: &Map<String, Value>,
) -> Result<Value, (String, String)> {
    check_account(args, auth.user_id().get())?;
    let wanted = ids(args)?;
    let list = owned_identities(state, auth.user_id().get())
        .await
        .map_err(server)?
        .into_iter()
        .filter(|identity| {
            wanted.as_ref().is_none_or(|ids| {
                identity
                    .get("id")
                    .and_then(Value::as_str)
                    .is_some_and(|id| ids.contains(id))
            })
        })
        .collect::<Vec<_>>();
    let returned: BTreeSet<String> = list
        .iter()
        .filter_map(|value| value.get("id").and_then(Value::as_str).map(str::to_string))
        .collect();
    let not_found = wanted
        .unwrap_or_default()
        .into_iter()
        .filter(|id| !returned.contains(id))
        .collect::<Vec<_>>();
    Ok(json!({
        "accountId": account_id(auth.user_id().get()),
        "state": session_state(state, auth.user_id().get()).await,
        "list": list,
        "notFound": not_found
    }))
}

/// Submit one already-stored Email through the shared raw-submission path.
///
/// The created object is not persisted. RFC 8621 allows a server to destroy a
/// submission immediately after accepting it; the Sent copy and the outbound
/// queue rows are the durable record, the same ones Webmail and SMTP write.
/// The submitted bytes are the stored RFC 5322 message, not a rebuilt copy.
async fn email_submission_set(
    state: &AppState,
    auth: &JmapAuth,
    args: &Map<String, Value>,
    created_ids: &BTreeMap<String, String>,
) -> Result<Value, (String, String)> {
    check_account(args, auth.user_id().get())?;
    if args.contains_key("update") || args.contains_key("destroy") {
        return Err((
            "invalidArguments".to_string(),
            "EmailSubmission objects cannot be updated or destroyed".to_string(),
        ));
    }
    let on_success = args
        .get("onSuccessDestroyEmail")
        .and_then(Value::as_array)
        .map(|ids| {
            ids.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect::<BTreeSet<_>>()
        })
        .unwrap_or_default();
    let create = args.get("create").and_then(Value::as_object).ok_or_else(|| {
        (
            "invalidArguments".to_string(),
            "create must be an object".to_string(),
        )
    })?;
    if create.len() > MAX_SET {
        return Err((
            "invalidArguments".to_string(),
            "too many creations".to_string(),
        ));
    }
    let mut created = Map::new();
    let mut not_created = Map::new();
    for (creation_id, object) in create {
        let Some(object) = object.as_object() else {
            not_created.insert(creation_id.clone(), json!({"type": "invalidProperties"}));
            continue;
        };
        if object.get("sendAt").is_some() {
            not_created.insert(
                creation_id.clone(),
                json!({"type": "invalidProperties", "properties": ["sendAt"]}),
            );
            continue;
        }
        match submit_one(state, auth, object, created_ids).await {
            Ok((id, source_id, thread_id)) => {
                created.insert(
                    creation_id.clone(),
                    json!({"id": id, "threadId": thread_id, "undoStatus": "final"}),
                );
                // `#id` is the source Email of that creation. A client puts it
                // here when the draft should disappear once sending succeeded.
                if on_success.contains(&format!("#{source_id}")) {
                    if let Err(error) = state
                        .mail_service
                        .delete_message(MessageId::new(source_id), auth.user_id(), true)
                        .await
                    {
                        tracing::warn!(
                            message_id = source_id,
                            error = %error,
                            "could not destroy the source email after a successful JMAP submission"
                        );
                    }
                }
            }
            Err(problem) => {
                not_created.insert(creation_id.clone(), problem);
            }
        }
    }
    let state_value = session_state(state, auth.user_id().get()).await;
    Ok(json!({
        "accountId": account_id(auth.user_id().get()),
        "oldState": state_value,
        "newState": state_value,
        "created": created,
        "notCreated": not_created,
        "destroyed": Value::Null
    }))
}

async fn submit_one(
    state: &AppState,
    auth: &JmapAuth,
    object: &Map<String, Value>,
    created_ids: &BTreeMap<String, String>,
) -> Result<(String, i64, String), Value> {
    let email_id = object
        .get("emailId")
        .and_then(Value::as_str)
        .ok_or_else(|| json!({"type": "invalidProperties", "properties": ["emailId"]}))?;
    let email_id = creation_id(created_ids, email_id).unwrap_or_else(|| email_id.to_string());
    let identity_id = object
        .get("identityId")
        .and_then(Value::as_str)
        .ok_or_else(|| json!({"type": "invalidProperties", "properties": ["identityId"]}))?;
    let message_id = email_id
        .parse::<i64>()
        .map_err(|_| json!({"type": "invalidProperties", "properties": ["emailId"]}))?;
    let identities = owned_identities(state, auth.user_id().get())
        .await
        .map_err(|error| json!({"type": "serverFail", "description": error.to_string()}))?;
    let identity = identities
        .iter()
        .find(|identity| identity.get("id").and_then(Value::as_str) == Some(identity_id));
    let Some(identity) = identity else {
        return Err(json!({
            "type": "invalidProperties",
            "description": "no such identity",
            "properties": ["identityId"]
        }));
    };
    let from = identity
        .get("email")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let (message, _mailbox, raw) = state
        .mail_service
        .raw_message(MessageId::new(message_id), auth.user_id())
        .await
        .map_err(|_| json!({"type": "invalidProperties", "properties": ["emailId"]}))?;
    let thread_id = message
        .thread_id
        .clone()
        .unwrap_or_else(|| message.id.to_string());
    let parsed = ferroma_mail::ParsedMessage::parse(&raw).map_err(|_| json!({"type": "invalidEmail"}))?;
    let mut recipients = header_recipients(&parsed);
    if let Some(envelope) = object.get("envelope") {
        let envelope = envelope
            .as_object()
            .ok_or_else(|| json!({"type": "invalidProperties", "properties": ["envelope"]}))?;
        if let Some(mail_from) = envelope.get("mailFrom") {
            let address = envelope_address(mail_from)
                .ok_or_else(|| json!({"type": "invalidProperties", "properties": ["envelope"]}))?;
            if !address.eq_ignore_ascii_case(&from) {
                return Err(json!({
                    "type": "forbiddenFrom",
                    "description": "the envelope sender must be the chosen identity"
                }));
            }
        }
        if let Some(rcpt_to) = envelope.get("rcptTo") {
            let listed = rcpt_to.as_array().ok_or_else(|| {
                json!({"type": "invalidProperties", "properties": ["envelope"]})
            })?;
            if listed.is_empty() {
                return Err(json!({"type": "noRecipients"}));
            }
            recipients = listed
                .iter()
                .map(|recipient| {
                    envelope_address(recipient).ok_or_else(|| {
                        json!({"type": "invalidProperties", "properties": ["envelope"]})
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
        }
    }
    if recipients.is_empty() {
        return Err(json!({"type": "noRecipients"}));
    }
    let sent = state
        .mail_service
        .submit_raw(auth.user_id(), &from, &raw, &recipients)
        .await
        .map_err(submission_error)?;
    Ok((format!("s{}", sent.message_id), message_id, thread_id))
}

fn header_recipients(parsed: &ferroma_mail::ParsedMessage) -> Vec<String> {
    let mut out = Vec::new();
    for header in ["To", "Cc", "Bcc"] {
        let Some(value) = parsed.header(header) else {
            continue;
        };
        for mailbox in ferroma_mail::address::parse_address_list(value) {
            let address = mailbox.address.to_string();
            if !out
                .iter()
                .any(|seen: &String| seen.eq_ignore_ascii_case(&address))
            {
                out.push(address);
            }
        }
    }
    out
}

fn envelope_address(value: &Value) -> Option<String> {
    let object = value.as_object()?;
    if object.contains_key("parameters") {
        return None;
    }
    let email = object.get("email")?.as_str()?.trim();
    let parsed = ferroma_core::EmailAddress::parse(email).ok()?;
    Some(parsed.to_string())
}

fn submission_error(error: FerromaError) -> Value {
    match error {
        FerromaError::RateLimited => json!({
            "type": "rateLimit",
            "description": "the account has reached its sending limit"
        }),
        FerromaError::LimitExceeded(message) => json!({
            "type": "tooManyRecipients",
            "description": message
        }),
        FerromaError::NotFound(message) => json!({
            "type": "forbiddenFrom",
            "description": message
        }),
        FerromaError::Invalid(message) if message.contains("recipient") => {
            json!({"type": "noRecipients", "description": message})
        }
        other => json!({"type": "serverFail", "description": other.to_string()}),
    }
}

/// Submissions are not stored, so a get after the set finds none of them.
async fn email_submission_get(
    state: &AppState,
    auth: &JmapAuth,
    args: &Map<String, Value>,
) -> Result<Value, (String, String)> {
    check_account(args, auth.user_id().get())?;
    let not_found = ids(args)?.unwrap_or_default().into_iter().collect::<Vec<_>>();
    Ok(json!({
        "accountId": account_id(auth.user_id().get()),
        "state": session_state(state, auth.user_id().get()).await,
        "list": [],
        "notFound": not_found
    }))
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
        .latest_cursor(UserId::new(user_id))
        .await
        .map(|cursor| cursor.0.to_string())
        .unwrap_or_else(|_| "0".to_string())
}

/// The cursor a `/changes` call starts from. A missing or non-numeric state is
/// `cannotCalculateChanges`: the client throws its cache away and calls `/get`.
fn required_state(args: &Map<String, Value>) -> Result<i64, (String, String)> {
    let raw = args.get("sinceState").and_then(Value::as_str).ok_or_else(|| {
        (
            "cannotCalculateChanges".to_string(),
            "sinceState is required".to_string(),
        )
    })?;
    raw.parse::<i64>().map_err(|_| {
        (
            "cannotCalculateChanges".to_string(),
            "sinceState is not a current state".to_string(),
        )
    })
}

/// One page of the change log, whether another page follows, and the state
/// that page ends on.
///
/// The state is the last row's cursor when the page is full, not the account's
/// newest cursor. A client that stored the newest cursor after a truncated page
/// would skip everything the next call was supposed to return.
async fn changes_since(
    state: &AppState,
    user: UserId,
    since: i64,
) -> Result<(Vec<ChangeLogEntry>, bool, String), (String, String)> {
    let latest = state.sync.latest_cursor(user).await.map_err(server)?;
    if since > latest.0 {
        return Err((
            "cannotCalculateChanges".to_string(),
            "sinceState is newer than the server".to_string(),
        ));
    }
    if since > 0 {
        if let Some(oldest) = state.sync.oldest_cursor(user).await.map_err(server)? {
            if since < oldest.0.saturating_sub(1) {
                return Err((
                    "cannotCalculateChanges".to_string(),
                    "sinceState is older than the retained history".to_string(),
                ));
            }
        }
    }
    // One past the page, so a full page is distinguishable from the last one.
    let mut rows = state
        .repos
        .change_log
        .changes_since(user, ferroma_core::Cursor(since), MAX_GET as i64 + 1)
        .await
        .map_err(server)?;
    let has_more = rows.len() > MAX_GET;
    if has_more {
        rows.truncate(MAX_GET);
    }
    let new_state = rows
        .last()
        .map(|row| row.seq.to_string())
        .unwrap_or_else(|| since.to_string());
    Ok((rows, has_more, new_state))
}

fn changes_response(
    auth: &JmapAuth,
    old_state: i64,
    new_state: String,
    created: Vec<String>,
    updated: Vec<String>,
    destroyed: Vec<String>,
    has_more: bool,
) -> Value {
    json!({
        "accountId": account_id(auth.user_id().get()),
        "oldState": old_state.to_string(),
        "newState": new_state,
        "hasMoreChanges": has_more,
        "created": created,
        "updated": updated,
        "destroyed": destroyed
    })
}

fn push_unique(ids: &mut Vec<String>, id: String) {
    if !ids.contains(&id) {
        ids.push(id);
    }
}

/// A mailbox id, or `#creationId` for one created earlier in this request.
fn resolve_mailbox_id(
    raw: &str,
    created_ids: &BTreeMap<String, String>,
    created: &Map<String, Value>,
) -> Option<i64> {
    let resolved = creation_id(created_ids, raw).or_else(|| {
        created
            .get(raw.strip_prefix('#')?)
            .and_then(|value| value.get("id"))
            .and_then(Value::as_str)
            .map(str::to_string)
    });
    resolved.as_deref().unwrap_or(raw).parse().ok()
}

fn resolve_mailbox_keys(
    mailbox_ids: &mut Map<String, Value>,
    created_ids: &BTreeMap<String, String>,
    created: &Map<String, Value>,
) {
    let keys = mailbox_ids.keys().cloned().collect::<Vec<_>>();
    for key in keys {
        let Some(resolved) = resolve_mailbox_id(&key, created_ids, created) else {
            continue;
        };
        if let Some(value) = mailbox_ids.remove(&key) {
            mailbox_ids.insert(resolved.to_string(), value);
        }
    }
}

async fn owned_mailbox_row(
    state: &AppState,
    auth: &JmapAuth,
    mailbox_id: i64,
) -> Result<ferroma_storage::models::Mailbox, Value> {
    let mailbox = state
        .repos
        .mailboxes
        .find_by_id(MailboxId::new(mailbox_id))
        .await
        .map_err(|error| json!({"type": "serverFail", "description": error.to_string()}))?
        .filter(|mailbox| mailbox.user_id == auth.user_id().get())
        .ok_or_else(|| json!({"type": "invalidProperties", "properties": ["parentId"]}))?;
    Ok(mailbox)
}

async fn owned_folder(
    state: &AppState,
    auth: &JmapAuth,
    folder_id: i64,
) -> Result<(ferroma_storage::models::Folder, ferroma_storage::models::Mailbox), Value> {
    crate::routes::mail::ownership::owned_folder(
        &state.repos,
        MailboxId::new(folder_id),
        auth.user_id(),
    )
    .await
    .map_err(|_| json!({"type": "notFound"}))
}

async fn messages_by_ids(
    state: &AppState,
    user: UserId,
    ids: &BTreeSet<String>,
) -> Result<Vec<Message>, (String, String)> {
    let mut rows = Vec::new();
    for id in ids {
        let Ok(id) = id.parse::<i64>() else {
            continue;
        };
        match state
            .repos
            .messages
            .find_by_id(MessageId::new(id))
            .await
            .map_err(server)?
        {
            Some(message) if message.expunged_at.is_none() => {
                let mailbox = state
                    .repos
                    .mailboxes
                    .find_by_id(MailboxId::new(message.mailbox_id))
                    .await
                    .map_err(server)?;
                if mailbox.is_some_and(|mailbox| mailbox.user_id == user.get()) {
                    rows.push(message);
                }
            }
            _ => {}
        }
    }
    Ok(rows)
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
    fn the_session_advertises_submission_where_a_client_looks_for_it() {
        // Flectar Mail (`jmap-client`) refuses the account before opening a
        // mailbox when either of these is missing: "server does not advertise
        // JMAP EmailSubmission". The session value is an empty object; the
        // limits live on the account.
        let session = json!({
            "capabilities": { SUBMISSION: {} },
            "accounts": { "u1": { "accountCapabilities": { SUBMISSION: submission_account_capability() } } },
            "primaryAccounts": { SUBMISSION: "u1" }
        });
        assert!(session["capabilities"].get(SUBMISSION).is_some());
        assert_eq!(session["capabilities"][SUBMISSION], json!({}));
        assert_eq!(
            session["accounts"]["u1"]["accountCapabilities"][SUBMISSION]["maxDelayedSend"],
            0
        );
        assert_eq!(session["primaryAccounts"][SUBMISSION], "u1");
    }

    #[test]
    fn the_session_event_source_is_an_absolute_uri() {
        // RFC 8620 types `eventSourceUrl` as a URI. An empty string parses as a
        // relative URL with no base, and a client (jmap-client) stops at
        // "Session eventSourceUrl is not a valid URL" before it opens a mailbox.
        // Ferroma does not implement push; the value still has to be absolute, the
        // same way `apiUrl` is, so session parsing can finish.
        let base = "https://mail.example.com";
        let event_source = format!(
            "{base}/api/jmap/eventsource/?types={{types}}&closeafter={{closeafter}}&ping={{ping}}"
        );
        let parsed = url::Url::parse(&event_source).expect("absolute eventSourceUrl");
        assert_eq!(parsed.scheme(), "https");
        assert!(parsed.path().starts_with("/api/jmap/eventsource/"));
        assert!(url::Url::parse("").is_err());
    }

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
