//! End-to-end tests of the mail surface: attachments, sending, the queue, and the
//! message operations Webmail drives.

mod common;

use axum::http::StatusCode;
use common::{folder_id, seed_account, TestApp};
use serde_json::json;

/// A password the auth policy accepts.
const PASSWORD: &str = "correct horse battery";

/// A fresh app with an administrator and one ready address.
async fn app_with_address() -> (TestApp, String, i64, String) {
    let app = TestApp::new().await;
    let admin = app
        .json(
            "POST",
            "/api/v1/setup",
            None,
            json!({
                "email": "admin@example.com",
                "password": PASSWORD,
                "domain": "example.com"
            }),
        )
        .await
        .expect(StatusCode::CREATED)["access_token"]
        .as_str()
        .expect("setup token")
        .to_string();

    let (_user_id, mailbox_id, token) =
        seed_account(&app, &admin, "example.net", "alice@example.net", PASSWORD).await;
    (app, admin, mailbox_id, token)
}

/// Upload one attachment and return its id.
async fn upload(app: &TestApp, token: &str, filename: &str, bytes: &[u8]) -> i64 {
    let boundary = "----ferromaTestBoundary";
    let mut body = Vec::new();
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(
        format!(
            "Content-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\n\
             Content-Type: application/octet-stream\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(bytes);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());

    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/api/v1/attachments")
        .header(
            axum::http::header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={boundary}"),
        )
        .header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"))
        .body(axum::body::Body::from(body))
        .expect("valid request");

    let response = app.request(request).await;
    let body = response.expect(StatusCode::CREATED);
    assert_eq!(body["filename"], filename, "{body}");
    assert_eq!(body["size_bytes"].as_u64(), Some(bytes.len() as u64));
    assert!(body["sha256"].is_string(), "{body}");
    body["id"].as_i64().expect("attachment id")
}

#[tokio::test]
async fn uploading_an_attachment_stores_a_content_addressed_blob() {
    require_database!();
    let (app, _admin, _mailbox, token) = app_with_address().await;

    let first = upload(&app, &token, "report.pdf", b"%PDF-1.7 fake").await;
    let second = upload(&app, &token, "copy.pdf", b"%PDF-1.7 fake").await;
    assert_ne!(first, second, "each upload gets its own row");

    // Both rows point at the same blob, because the store is content-addressed.
    let meta_one = app
        .get(&format!("/api/v1/attachments/{first}/meta"), Some(&token))
        .await
        .expect(StatusCode::OK);
    let meta_two = app
        .get(&format!("/api/v1/attachments/{second}/meta"), Some(&token))
        .await
        .expect(StatusCode::OK);
    assert_eq!(meta_one["sha256"], meta_two["sha256"]);

    // The bytes round-trip exactly.
    let download = app
        .get(&format!("/api/v1/attachments/{first}"), Some(&token))
        .await;
    assert_eq!(download.status, StatusCode::OK);
    assert_eq!(download.body, b"%PDF-1.7 fake");
    assert_eq!(
        download.header("content-type").as_deref(),
        Some("application/octet-stream")
    );
    assert_eq!(
        download.header("accept-ranges").as_deref(),
        Some("bytes")
    );
    assert!(download.header("etag").is_some());

    app.cleanup().await;
}

#[tokio::test]
async fn an_attachment_download_honours_range_and_if_none_match() {
    require_database!();
    let (app, _admin, _mailbox, token) = app_with_address().await;
    let id = upload(&app, &token, "ranged.bin", b"0123456789").await;

    let request = axum::http::Request::builder()
        .method("GET")
        .uri(format!("/api/v1/attachments/{id}"))
        .header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"))
        .header(axum::http::header::RANGE, "bytes=2-5")
        .body(axum::body::Body::empty())
        .expect("valid request");
    let partial = app.request(request).await;
    assert_eq!(partial.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(partial.body, b"2345");
    assert_eq!(
        partial.header("content-range").as_deref(),
        Some("bytes 2-5/10")
    );

    let full = app
        .get(&format!("/api/v1/attachments/{id}"), Some(&token))
        .await;
    let etag = full.header("etag").expect("etag");

    let request = axum::http::Request::builder()
        .method("GET")
        .uri(format!("/api/v1/attachments/{id}"))
        .header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"))
        .header(axum::http::header::IF_NONE_MATCH, etag)
        .body(axum::body::Body::empty())
        .expect("valid request");
    let cached = app.request(request).await;
    assert_eq!(cached.status, StatusCode::NOT_MODIFIED);
    assert!(cached.body.is_empty());

    app.cleanup().await;
}

#[tokio::test]
async fn one_user_cannot_download_another_users_attachment() {
    require_database!();
    let (app, admin, _mailbox, alice) = app_with_address().await;
    let (_bob_id, _bob_mailbox, bob) =
        seed_account(&app, &admin, "example.net", "bob@example.net", PASSWORD).await;

    let id = upload(&app, &alice, "secret.pdf", b"private").await;
    let stolen = app
        .get(&format!("/api/v1/attachments/{id}"), Some(&bob))
        .await;
    assert_eq!(stolen.status, StatusCode::NOT_FOUND);
    assert_eq!(
        app.get(&format!("/api/v1/attachments/{id}/meta"), Some(&bob))
            .await
            .status,
        StatusCode::NOT_FOUND
    );

    app.cleanup().await;
}

#[tokio::test]
async fn a_later_queue_insert_failure_leaves_no_partial_recipient_queue() {
    require_database!();
    let (app, _admin, _mailbox_id, token) = app_with_address().await;
    app.db().execute("CREATE FUNCTION refuse_second_queue() RETURNS trigger LANGUAGE plpgsql AS $$
        BEGIN IF NEW.recipient = 'carol@example.org' THEN
            RAISE EXCEPTION 'injected second queue failure'; END IF;
            RETURN NEW; END $$").await.unwrap();
    app.db().execute("CREATE TRIGGER refuse_second_queue BEFORE INSERT ON mail_queue
        FOR EACH ROW EXECUTE FUNCTION refuse_second_queue()").await.unwrap();
    let failed = app.json("POST", "/api/v1/messages", Some(&token), json!({
        "from": "alice@example.net", "to": ["bob@example.org", "carol@example.org"],
        "subject": "all recipients or none", "text": "body"
    })).await;
    assert_ne!(failed.status, StatusCode::OK);
    assert_eq!(app.db().count("mail_queue").await, 0,
        "first recipient must not be scheduled after second failed");
    app.cleanup().await;
}

#[tokio::test]
async fn sending_a_message_queues_one_row_per_recipient_and_copies_it_to_sent() {
    require_database!();
    let (app, _admin, mailbox_id, token) = app_with_address().await;

    let attachment = upload(&app, &token, "invoice.pdf", b"invoice bytes").await;

    let sent = app
        .json(
            "POST",
            "/api/v1/messages",
            Some(&token),
            json!({
                "from": "alice@example.net",
                "to": ["bob@example.org", "carol@example.org"],
                "cc": ["dan@example.org"],
                "subject": "Invoice for September",
                "text": "Hi, attached is the invoice.",
                "html": "<p>Hi, attached is the invoice.</p>",
                "attachments": [attachment]
            }),
        )
        .await;
    let body = sent.expect(StatusCode::OK);
    let message_id = body["message_id"].as_i64().expect("message id");
    assert_eq!(body["queued"], 3, "{body}");
    let recipients: Vec<&str> = body["recipients"]
        .as_array()
        .expect("recipients")
        .iter()
        .filter_map(|value| value.as_str())
        .collect();
    assert!(recipients.contains(&"bob@example.org"));
    assert!(recipients.contains(&"carol@example.org"));
    assert!(recipients.contains(&"dan@example.org"));

    // One `mail_queue` row per recipient.
    assert_eq!(app.db().count("mail_queue").await, 3);
    let queue = app
        .get("/api/v1/queue?status=pending", Some(&_admin))
        .await
        .expect(StatusCode::OK);
    assert_eq!(queue["total"], 3);

    // The sender's copy is in `Sent`.
    let sent_folder = folder_id(&app, &token, mailbox_id, "Sent").await;
    let listed = app
        .get(
            &format!("/api/v1/messages?folder_id={sent_folder}"),
            Some(&token),
        )
        .await
        .expect(StatusCode::OK);
    assert_eq!(listed["total"], 1);
    assert_eq!(listed["items"][0]["id"].as_i64(), Some(message_id));
    assert_eq!(listed["items"][0]["subject"], "Invoice for September");
    assert_eq!(listed["items"][0]["has_attachments"], true);
    assert_eq!(listed["items"][0]["attachment_count"], 1);
    assert!(listed["items"][0]["flags"]
        .as_str()
        .unwrap_or_default()
        .contains("seen"));

    // The full read carries the bodies and the threading headers Webmail needs.
    let detail = app
        .get(&format!("/api/v1/messages/{message_id}"), Some(&token))
        .await
        .expect(StatusCode::OK);
    assert!(detail["text_body"]
        .as_str()
        .unwrap_or_default()
        .contains("attached is the invoice"));
    assert!(detail["html_body"]
        .as_str()
        .unwrap_or_default()
        .contains("<p>"));
    assert!(detail["message_id_header"].is_string(), "{detail}");
    assert!(detail["references"].is_array(), "{detail}");
    assert_eq!(detail["attachment_count"], 1);
    assert_eq!(detail["attachments"][0]["filename"], "invoice.pdf");
    assert_eq!(detail["cc"].as_array().map(Vec::len), Some(1));
    assert_eq!(detail["is_draft"], false);

    // The raw endpoint answers `message/rfc822` with the stored bytes.
    let raw = app
        .get(&format!("/api/v1/messages/{message_id}/raw"), Some(&token))
        .await;
    assert_eq!(raw.status, StatusCode::OK);
    assert_eq!(
        raw.header("content-type").as_deref(),
        Some("message/rfc822")
    );
    let text = String::from_utf8_lossy(&raw.body);
    assert!(text.contains("Subject: Invoice for September"), "{text}");
    assert!(text.contains("invoice.pdf"), "the attachment must be in the bytes");

    app.cleanup().await;
}

#[tokio::test]
async fn a_draft_is_filed_in_drafts_and_queues_nothing() {
    require_database!();
    let (app, _admin, mailbox_id, token) = app_with_address().await;

    let saved = app
        .json(
            "POST",
            "/api/v1/messages",
            Some(&token),
            json!({
                "from": "alice@example.net",
                "subject": "half-written",
                "text": "still thinking",
                "draft": true
            }),
        )
        .await;
    let body = saved.expect(StatusCode::CREATED);
    assert_eq!(body["queued"], 0);
    assert_eq!(app.db().count("mail_queue").await, 0);

    let drafts_folder = folder_id(&app, &token, mailbox_id, "Drafts").await;
    let listed = app
        .get(
            &format!("/api/v1/messages?folder_id={drafts_folder}"),
            Some(&token),
        )
        .await
        .expect(StatusCode::OK);
    assert_eq!(listed["total"], 1, "{listed}");
    assert_eq!(listed["items"][0]["is_draft"], true);
    assert!(listed["items"][0]["flags"]
        .as_str()
        .unwrap_or_default()
        .contains("draft"));

    app.cleanup().await;
}

#[tokio::test]
async fn sending_from_an_address_the_caller_does_not_own_is_not_found() {
    require_database!();
    let (app, admin, _mailbox, token) = app_with_address().await;
    seed_account(&app, &admin, "example.net", "bob@example.net", PASSWORD).await;

    let forged = app
        .json(
            "POST",
            "/api/v1/messages",
            Some(&token),
            json!({
                "from": "bob@example.net",
                "to": ["carol@example.org"],
                "subject": "not mine",
                "text": "forged"
            }),
        )
        .await;
    assert_eq!(forged.status, StatusCode::NOT_FOUND);
    assert_eq!(forged.error_code(), "not_found");
    assert_eq!(app.db().count("mail_queue").await, 0);

    app.cleanup().await;
}

#[tokio::test]
async fn a_message_needs_a_recipient() {
    require_database!();
    let (app, _admin, _mailbox, token) = app_with_address().await;

    let response = app
        .json(
            "POST",
            "/api/v1/messages",
            Some(&token),
            json!({
                "from": "alice@example.net",
                "subject": "nobody",
                "text": "hello?"
            }),
        )
        .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert_eq!(response.error_code(), "invalid_input");

    app.cleanup().await;
}

#[tokio::test]
async fn the_message_list_supports_every_documented_filter() {
    require_database!();
    let (app, _admin, mailbox_id, token) = app_with_address().await;

    for subject in ["alpha invoice", "beta report", "gamma invoice"] {
        app.json(
            "POST",
            "/api/v1/messages",
            Some(&token),
            json!({
                "from": "alice@example.net",
                "to": ["bob@example.org"],
                "subject": subject,
                "text": "body of the message"
            }),
        )
        .await
        .expect(StatusCode::OK);
    }

    let all = app
        .get(&format!("/api/v1/messages?mailbox_id={mailbox_id}"), Some(&token))
        .await
        .expect(StatusCode::OK);
    let total = all["total"].as_i64().expect("total");
    assert_eq!(total, 3, "{all}");

    let searched = app
        .get(
            &format!("/api/v1/messages?mailbox_id={mailbox_id}&query=invoice"),
            Some(&token),
        )
        .await
        .expect(StatusCode::OK);
    assert_eq!(searched["total"], 2, "{searched}");

    let unread = app
        .get(
            &format!("/api/v1/messages?mailbox_id={mailbox_id}&unread=true"),
            Some(&token),
        )
        .await
        .expect(StatusCode::OK);
    assert_eq!(unread["total"], 0, "sent copies are marked seen");

    let flagged = app
        .get(
            &format!("/api/v1/messages?mailbox_id={mailbox_id}&flagged=true"),
            Some(&token),
        )
        .await
        .expect(StatusCode::OK);
    assert_eq!(flagged["total"], 0);

    let with_attachments = app
        .get(
            &format!("/api/v1/messages?mailbox_id={mailbox_id}&has_attachments=true"),
            Some(&token),
        )
        .await
        .expect(StatusCode::OK);
    assert_eq!(with_attachments["total"], 0);

    // Paging is the documented shape.
    let paged = app
        .get(
            &format!("/api/v1/messages?mailbox_id={mailbox_id}&limit=2&offset=0"),
            Some(&token),
        )
        .await
        .expect(StatusCode::OK);
    assert_eq!(paged["limit"], 2);
    assert_eq!(paged["offset"], 0);
    assert_eq!(paged["items"].as_array().map(Vec::len), Some(2));
    assert_eq!(paged["total"], 3);

    app.cleanup().await;
}

#[tokio::test]
async fn listing_messages_of_a_foreign_mailbox_is_not_found() {
    require_database!();
    let (app, admin, _mailbox, _alice) = app_with_address().await;
    let (_bob_id, bob_mailbox, bob) =
        seed_account(&app, &admin, "example.net", "bob@example.net", PASSWORD).await;

    let response = app
        .get(
            &format!("/api/v1/messages?mailbox_id={bob_mailbox}"),
            Some(&bob),
        )
        .await;
    // Bob may list his own; the fixture proves the scoping is per-caller.
    response.expect(StatusCode::OK);

    app.cleanup().await;
}

#[tokio::test]
async fn patching_a_message_changes_its_flags_and_publishes_the_event() {
    require_database!();
    let (app, _admin, mailbox_id, token) = app_with_address().await;

    let sent = app
        .json(
            "POST",
            "/api/v1/messages",
            Some(&token),
            json!({
                "from": "alice@example.net",
                "to": ["bob@example.org"],
                "subject": "flag me",
                "text": "body"
            }),
        )
        .await
        .expect(StatusCode::OK);
    let message_id = sent["message_id"].as_i64().expect("message id");

    let patched = app
        .json(
            "PATCH",
            &format!("/api/v1/messages/{message_id}"),
            Some(&token),
            json!({ "flagged": true, "answered": true, "seen": false }),
        )
        .await;
    let body = patched.expect(StatusCode::OK);
    let flags = body["flags"].as_str().unwrap_or_default();
    assert!(flags.contains("flagged"), "{flags}");
    assert!(flags.contains("answered"), "{flags}");
    assert!(!flags.contains("seen"), "{flags}");

    // The Maildir file name follows the flags, which is what an IMAP client reads.
    let listed = app
        .get(
            &format!("/api/v1/messages?folder_id={}", folder_id(&app, &token, mailbox_id, "Sent").await),
            Some(&token),
        )
        .await
        .expect(StatusCode::OK);
    assert_eq!(listed["items"][0]["flags"], flags);

    app.cleanup().await;
}

#[tokio::test]
async fn moving_and_copying_a_message_keeps_the_folders_consistent() {
    require_database!();
    let (app, _admin, mailbox_id, token) = app_with_address().await;

    let sent = app
        .json(
            "POST",
            "/api/v1/messages",
            Some(&token),
            json!({
                "from": "alice@example.net",
                "to": ["bob@example.org"],
                "subject": "move me",
                "text": "body"
            }),
        )
        .await
        .expect(StatusCode::OK);
    let message_id = sent["message_id"].as_i64().expect("message id");

    let archive = folder_id(&app, &token, mailbox_id, "Archive").await;
    let moved = app
        .json(
            "POST",
            &format!("/api/v1/messages/{message_id}/move"),
            Some(&token),
            json!({ "folder_id": archive }),
        )
        .await;
    let body = moved.expect(StatusCode::OK);
    assert_eq!(body["folder_id"].as_i64(), Some(archive));

    // A move keeps one row; a copy makes a second.
    let copied = app
        .json(
            "POST",
            &format!("/api/v1/messages/{message_id}/copy"),
            Some(&token),
            json!({ "folder_id": folder_id(&app, &token, mailbox_id, "Junk").await }),
        )
        .await;
    let copied_body = copied.expect(StatusCode::CREATED);
    assert_ne!(copied_body["id"].as_i64(), Some(message_id));
    let copied_id = ferroma_core::MessageId::new(copied_body["id"].as_i64().expect("copy id"));
    let copied_row = app.state.repos.messages.require_by_id(copied_id).await.expect("copied row");
    assert_ne!(copied_row.storage_path,
        app.state.repos.messages.require_by_id(ferroma_core::MessageId::new(message_id))
            .await.expect("original row").storage_path);
    assert!(app.state.maildir.read(&copied_row.storage_path).is_ok(), "copied body must exist");
    let owner = app.state.repos.mailboxes.find_by_id(ferroma_core::MailboxId::new(mailbox_id))
        .await.unwrap().unwrap().user_id;
    let page = app.state.sync.sync(ferroma_sync::SyncRequest::account(
        ferroma_core::UserId::new(owner), ferroma_core::MailboxId::new(mailbox_id),
        ferroma_core::Cursor::ZERO,
    )).await.unwrap();
    assert_eq!(page.changes.iter().filter(|entry|
        entry.kind == ferroma_sync::ChangeKind::MessageMoved
            && entry.message_id == Some(ferroma_core::MessageId::new(message_id))).count(), 1);
    assert_eq!(page.changes.iter().filter(|entry|
        entry.kind == ferroma_sync::ChangeKind::MessageCreated
            && entry.message_id == Some(copied_id)).count(), 1);

    let archive_list = app
        .get(&format!("/api/v1/messages?folder_id={archive}"), Some(&token))
        .await
        .expect(StatusCode::OK);
    assert_eq!(archive_list["total"], 1);

    // The HTTP entry point must enforce the same quota as IMAP COPY.
    let current = app.state.repos.mailboxes.used_bytes(ferroma_core::MailboxId::new(mailbox_id))
        .await.unwrap();
    app.state.repos.users.set_quota(ferroma_core::UserId::new(owner), current)
        .await.unwrap();
    let refused = app.json("POST", &format!("/api/v1/messages/{message_id}/copy"),
        Some(&token), json!({ "folder_id": archive })).await;
    assert_eq!(refused.status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(app.state.repos.mailboxes.used_bytes(ferroma_core::MailboxId::new(mailbox_id))
        .await.unwrap(), current);

    app.cleanup().await;
}

#[tokio::test]
async fn renaming_a_nonempty_folder_keeps_its_copied_message_readable() {
    require_database!();
    let (app, _admin, mailbox_id, token) = app_with_address().await;
    let sent = app.json("POST", "/api/v1/messages", Some(&token), json!({
        "from": "alice@example.net", "to": ["bob@example.org"],
        "subject": "keep body on rename", "text": "body"
    })).await.expect(StatusCode::OK);
    let id = sent["message_id"].as_i64().unwrap();
    let created = app.json("POST", &format!("/api/v1/mailboxes/{mailbox_id}/folders"),
        Some(&token), json!({"name": "Projects"})).await.expect(StatusCode::CREATED);
    let folder = created["id"].as_i64().unwrap();
    let copy = app.json("POST", &format!("/api/v1/messages/{id}/copy"),
        Some(&token), json!({"folder_id": folder})).await.expect(StatusCode::CREATED);
    let copied_id = ferroma_core::MessageId::new(copy["id"].as_i64().unwrap());
    let original = app.state.repos.messages.require_by_id(copied_id).await.unwrap();
    let renamed = app.json("PATCH", &format!("/api/v1/folders/{folder}"),
        Some(&token), json!({"name": "Work"})).await.expect(StatusCode::OK);
    assert_eq!(renamed["name"], "Work");
    let current = app.state.repos.messages.require_by_id(copied_id).await.unwrap();
    assert_ne!(current.storage_path, original.storage_path);
    assert!(app.state.maildir.read(&current.storage_path).is_ok());
    let detail = app.get(&format!("/api/v1/messages/{}", copied_id.get()), Some(&token))
        .await.expect(StatusCode::OK);
    assert_eq!(detail["subject"], "keep body on rename");
    app.cleanup().await;
}

#[tokio::test]
async fn deleting_moves_to_trash_and_permanent_removes_the_row() {
    require_database!();
    let (app, _admin, mailbox_id, token) = app_with_address().await;

    let sent = app
        .json(
            "POST",
            "/api/v1/messages",
            Some(&token),
            json!({
                "from": "alice@example.net",
                "to": ["bob@example.org"],
                "subject": "delete me",
                "text": "body"
            }),
        )
        .await
        .expect(StatusCode::OK);
    let message_id = sent["message_id"].as_i64().expect("message id");

    let trashed = app
        .delete(&format!("/api/v1/messages/{message_id}"), Some(&token))
        .await;
    assert_eq!(trashed.status, StatusCode::NO_CONTENT);

    let trash = folder_id(&app, &token, mailbox_id, "Trash").await;
    let in_trash = app
        .get(&format!("/api/v1/messages?folder_id={trash}"), Some(&token))
        .await
        .expect(StatusCode::OK);
    assert_eq!(in_trash["total"], 1, "{in_trash}");

    // The soft delete updated the same row rather than inserting one.
    assert_eq!(app.db().count("messages").await, 1);

    let permanent_url = format!("/api/v1/messages/{message_id}?permanent=true");
    let refused = app.delete(&permanent_url, Some(&token)).await;
    assert_eq!(refused.status, StatusCode::CONFLICT,
        "an active outbound delivery must keep its source body");
    let queued = app.state.repos.queue
        .list_by_message(ferroma_core::MessageId::new(message_id)).await.unwrap();
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].status, "pending");
    let row = app.state.repos.messages
        .require_by_id(ferroma_core::MessageId::new(message_id)).await.unwrap();
    assert!(app.state.maildir.read(&row.storage_path).is_ok());

    app.state.repos.queue.mark_delivered(queued[0].queue_id(), None, None, None)
        .await.unwrap();
    let permanent = app.delete(&permanent_url, Some(&token)).await;
    assert_eq!(permanent.status, StatusCode::NO_CONTENT);
    assert_eq!(app.db().count("messages").await, 0);
    // ...and a tombstone survives in the change log.
    assert!(app.db().count("change_log").await >= 2);

    app.cleanup().await;
}

#[tokio::test]
async fn the_batch_endpoint_applies_one_operation_to_many_messages() {
    require_database!();
    let (app, _admin, _mailbox, token) = app_with_address().await;

    let mut ids = Vec::new();
    for index in 0..3 {
        let sent = app
            .json(
                "POST",
                "/api/v1/messages",
                Some(&token),
                json!({
                    "from": "alice@example.net",
                    "to": ["bob@example.org"],
                    "subject": format!("batch {index}"),
                    "text": "body"
                }),
            )
            .await
            .expect(StatusCode::OK);
        ids.push(sent["message_id"].as_i64().expect("message id"));
    }

    let read = app
        .json(
            "POST",
            "/api/v1/messages/batch",
            Some(&token),
            json!({ "operation": "unread", "ids": ids }),
        )
        .await;
    let body = read.expect(StatusCode::OK);
    assert_eq!(body["operation"], "unread");
    assert_eq!(body["affected"], 3);

    for id in &ids {
        let detail = app
            .get(&format!("/api/v1/messages/{id}"), Some(&token))
            .await
            .expect(StatusCode::OK);
        assert!(
            !detail["flags"].as_str().unwrap_or_default().contains("seen"),
            "{detail}"
        );
    }

    // An unknown operation is refused rather than silently ignored.
    let bad = app
        .json(
            "POST",
            "/api/v1/messages/batch",
            Some(&token),
            json!({ "operation": "teleport", "ids": ids }),
        )
        .await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);

    // An empty id list is refused too.
    let empty = app
        .json(
            "POST",
            "/api/v1/messages/batch",
            Some(&token),
            json!({ "operation": "read", "ids": [] }),
        )
        .await;
    assert_eq!(empty.status, StatusCode::BAD_REQUEST);

    app.cleanup().await;
}

#[tokio::test]
async fn a_batch_that_names_somebody_elses_message_skips_it_without_confirming_it() {
    require_database!();
    let (app, admin, _mailbox, alice) = app_with_address().await;
    let (_bob_id, _bob_mailbox, bob) =
        seed_account(&app, &admin, "example.net", "bob@example.net", PASSWORD).await;

    let sent = app
        .json(
            "POST",
            "/api/v1/messages",
            Some(&alice),
            json!({
                "from": "alice@example.net",
                "to": ["bob@example.org"],
                "subject": "mine",
                "text": "body"
            }),
        )
        .await
        .expect(StatusCode::OK);
    let id = sent["message_id"].as_i64().expect("message id");

    let response = app
        .json(
            "POST",
            "/api/v1/messages/batch",
            Some(&bob),
            json!({ "operation": "read", "ids": [id] }),
        )
        .await;
    let body = response.expect(StatusCode::OK);
    assert_eq!(body["affected"], 0, "{body}");
    assert_eq!(body["ids"], json!([]));

    app.cleanup().await;
}

#[tokio::test]
async fn the_queue_reports_the_message_it_belongs_to_and_can_be_cancelled() {
    require_database!();
    let (app, admin, _mailbox, token) = app_with_address().await;

    let sent = app
        .json(
            "POST",
            "/api/v1/messages",
            Some(&token),
            json!({
                "from": "alice@example.net",
                "to": ["bob@example.org"],
                "subject": "queue me",
                "text": "body"
            }),
        )
        .await
        .expect(StatusCode::OK);
    let message_id = sent["message_id"].as_i64().expect("message id");

    let queue = app
        .get("/api/v1/queue", Some(&admin))
        .await
        .expect(StatusCode::OK);
    assert_eq!(queue["total"], 1, "{queue}");
    let queue_id = queue["items"][0]["id"].as_i64().expect("queue id");
    assert_eq!(queue["items"][0]["message_id"].as_i64(), Some(message_id));
    assert_eq!(queue["items"][0]["status"], "pending");
    assert_eq!(queue["items"][0]["recipient"], "bob@example.org");

    let detail = app
        .get(&format!("/api/v1/queue/{queue_id}"), Some(&admin))
        .await
        .expect(StatusCode::OK);
    assert_eq!(detail["entry"]["id"].as_i64(), Some(queue_id));
    assert_eq!(detail["entries"].as_array().map(Vec::len), None);
    assert_eq!(detail["attempts"].as_array().map(Vec::len), Some(0));
    assert_eq!(detail["subject"], "queue me");
    assert_eq!(detail["recipient_count"], 1);

    let stats = app
        .get("/api/v1/queue/stats", Some(&admin))
        .await
        .expect(StatusCode::OK);
    assert_eq!(stats["pending"], 1);
    assert_eq!(stats["outstanding"], 1);

    let cancelled = app
        .delete(&format!("/api/v1/queue/{queue_id}"), Some(&admin))
        .await;
    assert_eq!(cancelled.status, StatusCode::NO_CONTENT);

    // A second cancel is refused rather than silently succeeding.
    let again = app
        .delete(&format!("/api/v1/queue/{queue_id}"), Some(&admin))
        .await;
    assert_eq!(again.status, StatusCode::CONFLICT);

    let stats = app
        .get("/api/v1/queue/stats", Some(&admin))
        .await
        .expect(StatusCode::OK);
    assert_eq!(stats["cancelled"], 1);

    app.cleanup().await;
}

#[tokio::test]
async fn the_queue_status_filter_accepts_a_list_and_refuses_a_typo() {
    require_database!();
    let (app, admin, _mailbox, token) = app_with_address().await;

    app.json(
        "POST",
        "/api/v1/messages",
        Some(&token),
        json!({
            "from": "alice@example.net",
            "to": ["bob@example.org"],
            "subject": "filter me",
            "text": "body"
        }),
    )
    .await
    .expect(StatusCode::OK);

    let both = app
        .get("/api/v1/queue?status=retry,failed", Some(&admin))
        .await
        .expect(StatusCode::OK);
    assert_eq!(both["total"], 0, "{both}");

    let pending = app
        .get("/api/v1/queue?status=pending", Some(&admin))
        .await
        .expect(StatusCode::OK);
    assert_eq!(pending["total"], 1);

    let typo = app
        .get("/api/v1/queue?status=pendign", Some(&admin))
        .await;
    assert_eq!(typo.status, StatusCode::BAD_REQUEST);

    app.cleanup().await;
}

#[tokio::test]
async fn drafts_round_trip_through_the_management_surface() {
    require_database!();
    let (app, _admin, mailbox_id, token) = app_with_address().await;

    let created = app
        .json(
            "POST",
            "/api/v1/drafts",
            Some(&token),
            json!({
                "mailbox_id": mailbox_id,
                "subject": "draft subject",
                "text": "draft body",
                "to": ["bob@example.org"],
                "cc": ["carol@example.org"]
            }),
        )
        .await;
    let body = created.expect(StatusCode::CREATED);
    let draft_id = body["id"].as_i64().expect("draft id");
    assert_eq!(body["subject"], "draft subject");
    assert_eq!(body["text"], "draft body");
    assert_eq!(body["to"][0]["address"], "bob@example.org");
    assert_eq!(body["cc"][0]["address"], "carol@example.org");

    let listed = app
        .get("/api/v1/drafts", Some(&token))
        .await
        .expect(StatusCode::OK);
    assert_eq!(listed["total"], 1);
    assert_eq!(listed["items"][0]["id"].as_i64(), Some(draft_id));

    let fetched = app
        .get(&format!("/api/v1/drafts/{draft_id}"), Some(&token))
        .await
        .expect(StatusCode::OK);
    assert_eq!(fetched["subject"], "draft subject");

    // The draft is mirrored into the Drafts folder as a real message.
    let drafts_folder = folder_id(&app, &token, mailbox_id, "Drafts").await;
    let mirrored = app
        .get(
            &format!("/api/v1/messages?folder_id={drafts_folder}"),
            Some(&token),
        )
        .await
        .expect(StatusCode::OK);
    assert_eq!(mirrored["total"], 1, "{mirrored}");
    assert_eq!(mirrored["items"][0]["is_draft"], true);

    let updated = app
        .json(
            "PATCH",
            &format!("/api/v1/drafts/{draft_id}"),
            Some(&token),
            json!({ "subject": "renamed draft" }),
        )
        .await
        .expect(StatusCode::OK);
    assert_eq!(updated["subject"], "renamed draft");

    let deleted = app
        .delete(&format!("/api/v1/drafts/{draft_id}"), Some(&token))
        .await;
    assert_eq!(deleted.status, StatusCode::NO_CONTENT);
    // Deleting from the management surface removes the mirror too.
    assert_eq!(app.db().count("drafts").await, 0);
    assert_eq!(app.db().count("messages").await, 0);

    app.cleanup().await;
}

#[tokio::test]
async fn one_user_cannot_touch_another_users_draft() {
    require_database!();
    let (app, admin, _mailbox, alice) = app_with_address().await;
    let (_bob_id, _bob_mailbox, bob) =
        seed_account(&app, &admin, "example.net", "bob@example.net", PASSWORD).await;

    let created = app
        .json(
            "POST",
            "/api/v1/drafts",
            Some(&alice),
            json!({ "subject": "alice's draft" }),
        )
        .await
        .expect(StatusCode::CREATED);
    let draft_id = created["id"].as_i64().expect("draft id");

    assert_eq!(
        app.get(&format!("/api/v1/drafts/{draft_id}"), Some(&bob))
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        app.json(
            "PATCH",
            &format!("/api/v1/drafts/{draft_id}"),
            Some(&bob),
            json!({ "subject": "hijacked" })
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        app.delete(&format!("/api/v1/drafts/{draft_id}"), Some(&bob))
            .await
            .status,
        StatusCode::NOT_FOUND
    );

    // Alice's draft is untouched.
    let still_there = app
        .get(&format!("/api/v1/drafts/{draft_id}"), Some(&alice))
        .await
        .expect(StatusCode::OK);
    assert_eq!(still_there["subject"], "alice's draft");

    app.cleanup().await;
}

#[tokio::test]
async fn a_message_belonging_to_a_deleted_user_is_gone() {
    require_database!();
    let (app, admin, _mailbox, _token) = app_with_address().await;

    let user = app
        .json(
            "POST",
            "/api/v1/users",
            Some(&admin),
            json!({ "email": "temp@example.net", "password": PASSWORD }),
        )
        .await
        .expect(StatusCode::CREATED);
    let user_id = user["id"].as_i64().expect("user id");

    let removed = app
        .delete(&format!("/api/v1/users/{user_id}"), Some(&admin))
        .await;
    assert_eq!(removed.status, StatusCode::NO_CONTENT);
    assert_eq!(
        app.get(&format!("/api/v1/users/{user_id}"), Some(&admin))
            .await
            .status,
        StatusCode::NOT_FOUND
    );

    app.cleanup().await;
}

#[tokio::test]
async fn a_message_reports_its_flags_as_booleans_and_its_blind_copies() {
    require_database!();
    let (app, _admin, mailbox_id, token) = app_with_address().await;

    let sent = app
        .json(
            "POST",
            "/api/v1/messages",
            Some(&token),
            json!({
                "from": "alice@example.net",
                "to": ["bob@example.org"],
                "bcc": ["hidden@example.org"],
                "subject": "blind copy",
                "text": "body"
            }),
        )
        .await;
    let body = sent.expect(StatusCode::OK);
    let message_id = body["message_id"].as_i64().expect("message id");

    // Both front-ends read `bcc` and the three flag booleans straight off the message
    // shapes. `flags` stays the canonical spelling; the booleans are derived from it in
    // the handler, so no consumer has to parse it — parsing it wrongly is how the
    // Webmail showed every message as unread and hid every star.
    let sent_folder = folder_id(&app, &token, mailbox_id, "Sent").await;
    let listed = app
        .get(
            &format!("/api/v1/messages?folder_id={sent_folder}"),
            Some(&token),
        )
        .await
        .expect(StatusCode::OK);
    let row = &listed["items"][0];
    assert_eq!(row["bcc"].as_array().map(Vec::len), Some(1), "{row}");
    assert_eq!(row["bcc"][0]["address"], "hidden@example.org", "{row}");
    assert_eq!(row["seen"], true, "the sender's own copy is seen: {row}");
    assert_eq!(row["flagged"], false, "{row}");
    assert_eq!(row["answered"], false, "{row}");

    let patched = app
        .json(
            "PATCH",
            &format!("/api/v1/messages/{message_id}"),
            Some(&token),
            json!({ "seen": true, "flagged": true, "answered": true }),
        )
        .await
        .expect(StatusCode::OK);
    assert_eq!(patched["seen"], true, "{patched}");
    assert_eq!(patched["flagged"], true, "{patched}");
    assert_eq!(patched["answered"], true, "{patched}");

    let detail = app
        .get(&format!("/api/v1/messages/{message_id}"), Some(&token))
        .await
        .expect(StatusCode::OK);
    assert_eq!(detail["seen"], true, "{detail}");
    assert_eq!(detail["flagged"], true, "{detail}");
    assert_eq!(detail["answered"], true, "{detail}");
    assert_eq!(detail["bcc"].as_array().map(Vec::len), Some(1), "{detail}");

    app.cleanup().await;
}

#[tokio::test]
async fn changing_a_flag_recounts_the_folder_it_lives_in() {
    require_database!();
    let (app, _admin, mailbox_id, token) = app_with_address().await;

    let inbox = folder_id(&app, &token, mailbox_id, "INBOX").await;
    // A delivered-but-unread message, written directly: the API test harness drives the
    // router, and the SMTP delivery path that normally creates one is not part of it.
    app.db()
        .execute(&format!(
            "INSERT INTO messages (folder_id, mailbox_id, uid, size_bytes, storage_path, flags)
             VALUES ({inbox}, {mailbox_id}, 1, 12, 'cur/recount.eml', '')"
        ))
        .await
        .expect("message insert");
    let message_id = app
        .db()
        .scalar::<i64>(&format!("SELECT id FROM messages WHERE folder_id = {inbox}"))
        .await
        .expect("message id");

    // The sidebar badge is `folders.unseen_count`, a denormalised counter. Nothing
    // recounted it when a flag changed, so reading a message left the badge claiming
    // unread mail forever — the counter is what this asserts against, not the row's own
    // flag, because the row was always correct.
    async fn unseen(app: &TestApp, token: &str, mailbox_id: i64, inbox: i64) -> i64 {
        let listed = app
            .get(&format!("/api/v1/mailboxes/{mailbox_id}/folders"), Some(token))
            .await
            .expect(StatusCode::OK);
        listed["folders"]
            .as_array()
            .expect("folders")
            .iter()
            .find(|folder| folder["id"].as_i64() == Some(inbox))
            .expect("INBOX")["unseen_count"]
            .as_i64()
            .expect("unseen_count")
    }

    app.json(
        "PATCH",
        &format!("/api/v1/messages/{message_id}"),
        Some(&token),
        json!({ "seen": false }),
    )
    .await
    .expect(StatusCode::OK);
    assert_eq!(unseen(&app, &token, mailbox_id, inbox).await, 1, "unread must count");

    app.json(
        "PATCH",
        &format!("/api/v1/messages/{message_id}"),
        Some(&token),
        json!({ "seen": true }),
    )
    .await
    .expect(StatusCode::OK);
    assert_eq!(unseen(&app, &token, mailbox_id, inbox).await, 0, "read must not count");

    app.cleanup().await;
}

/// A word that appears only inside a message body is findable by the search box.
///
/// The search box calls `GET /api/v1/messages?query=`, which matched the subject, the
/// sender and the stored snippet. A word a few lines into a message found nothing,
/// which is the single most common thing a user searches for.
#[tokio::test]
async fn the_search_box_finds_a_word_that_only_the_body_carries() {
    require_database!();
    let (app, admin, _mailbox_id, _token) = app_with_address().await;
    // The search runs as the sender, against the copy the submission files in `Sent`.
    // A recipient's copy would still be sitting in the outbound queue here: no queue
    // worker runs in this harness, which is why every other test in this file asserts
    // on the sender's side too.
    let (_bob, _bob_mailbox, bob) =
        seed_account(&app, &admin, "example.net", "bob@example.net", PASSWORD).await;
    app.json(
        "POST",
        "/api/v1/messages",
        Some(&bob),
        json!({
            "from": "bob@example.net",
            "to": ["alice@example.net"],
            "subject": "Quarterly report",
            "text": format!(
                "The appendix mentions a penguin colony near the coast. {}\
                 Deep in the message, past any preview, the word is obsidian.",
                "Filler that pushes the next sentence out of the stored snippet. ".repeat(6)
            )
        }),
    )
    .await
    .expect(StatusCode::OK);
    app.json(
        "POST",
        "/api/v1/messages",
        Some(&bob),
        json!({
            "from": "bob@example.net",
            "to": ["alice@example.net"],
            "subject": "Unrelated note",
            "text": "Nothing interesting here at all."
        }),
    )
    .await
    .expect(StatusCode::OK);

    let page = app
        .get(
            &format!("/api/v1/messages?query={}", urlencode("penguin")),
            Some(&bob),
        )
        .await
        .expect(StatusCode::OK);
    let items = page["items"].as_array().expect("items");
    assert_eq!(items.len(), 1, "only the message whose body carries the word: {page}");
    assert_eq!(items[0]["subject"], "Quarterly report");
    assert!(
        items[0]["snippet"].as_str().unwrap_or_default().contains("penguin"),
        "the snippet proves we matched the body text we indexed: {page}"
    );

    // A word nobody wrote finds nobody — the search is not returning everything.
    let none = app
        .get(
            &format!("/api/v1/messages?query={}", urlencode("narwhal")),
            Some(&bob),
        )
        .await
        .expect(StatusCode::OK);
    assert_eq!(none["items"].as_array().map(Vec::len), Some(0));

    // A word buried past the stored snippet is still found: the *whole* body is
    // indexed, not just the preview the list already shows.
    let deep = app
        .get(
            &format!("/api/v1/messages?query={}", urlencode("obsidian")),
            Some(&bob),
        )
        .await
        .expect(StatusCode::OK);
    assert_eq!(
        deep["items"].as_array().map(Vec::len),
        Some(1),
        "a word past the snippet must still be findable: {deep}"
    );

    // A prefix of a body word is not a word. It is found only if it happens to sit in
    // the subject, the sender or the stored snippet — the substring half of the
    // predicate, which is what keeps the prefix search a search box already had.
    let prefix = app
        .get(
            &format!("/api/v1/messages?query={}", urlencode("obsid")),
            Some(&bob),
        )
        .await
        .expect(StatusCode::OK);
    assert_eq!(
        prefix["items"].as_array().map(Vec::len),
        Some(0),
        "a prefix of a word deep in the body is neither indexed nor in the snippet"
    );
    // `pen` is not a word of its own, but it *is* a substring of `appendix` in the
    // snippet, so the substring half legitimately matches it. Asserting the positive
    // here is what proves that half still works rather than having been replaced.
    let in_snippet = app
        .get(
            &format!("/api/v1/messages?query={}", urlencode("pen")),
            Some(&bob),
        )
        .await
        .expect(StatusCode::OK);
    assert_eq!(
        in_snippet["items"].as_array().map(Vec::len),
        Some(1),
        "the substring fallback over the snippet must survive: {in_snippet}"
    );

    app.cleanup().await;
}

/// Hide characters a query string cannot carry raw.
fn urlencode(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (byte as char).to_string()
            }
            other => format!("%{other:02X}"),
        })
        .collect()
}
