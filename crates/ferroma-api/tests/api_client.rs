//! End-to-end tests of the Ferroma Client Protocol: sync cursors, idempotent
//! operations, device management, version negotiation and the realtime framing.

mod common;

use axum::http::StatusCode;
use common::{folder_id, login, seed_account, TestApp};
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

/// Log a client installation in, returning `(access_token, refresh_token, device_id)`.
async fn client_login(app: &TestApp, email: &str, password: &str) -> (String, String, i64) {
    let response = app
        .client_json(
            "POST",
            "/api/v1/client/auth/login",
            None,
            json!({
                "email": email,
                "password": password,
                "device": {
                    "device_uid": "install-3f2c",
                    "name": "Alice's laptop",
                    "platform": "windows",
                    "client_version": "0.7.0"
                }
            }),
        )
        .await;
    let body = response.expect(StatusCode::OK);
    assert_eq!(body["token_type"], "Bearer");
    assert!(body["expires_in"].as_u64().unwrap_or(0) > 0);
    (
        body["access_token"]
            .as_str()
            .expect("access token")
            .to_string(),
        body["refresh_token"]
            .as_str()
            .expect("refresh token")
            .to_string(),
        body["device_id"].as_i64().expect("device id"),
    )
}

#[tokio::test]
async fn a_client_login_registers_the_device_and_the_response_negotiates_the_version() {
    require_database!();
    let (app, _admin, _mailbox, _token) = app_with_address().await;

    let login_response = app
        .client_json(
            "POST",
            "/api/v1/client/auth/login",
            None,
            json!({
                "email": "alice@example.net",
                "password": PASSWORD,
                "device": {
                    "device_uid": "install-3f2c",
                    "name": "Alice's laptop",
                    "platform": "windows",
                    "client_version": "0.7.0"
                }
            }),
        )
        .await;
    let body = login_response.expect(StatusCode::OK);
    assert!(body["device_id"].as_i64().unwrap_or(0) > 0);

    // The version negotiation headers are on the response, as `fcp.md` §1 requires.
    assert_eq!(
        login_response.header("x-ferroma-protocol").as_deref(),
        Some("1")
    );
    assert!(login_response.header("x-ferroma-server").is_some());

    // The device row exists and is not revoked.
    assert_eq!(app.db().count("devices").await, 1);

    app.cleanup().await;
}

#[tokio::test]
async fn the_account_blob_reports_the_negotiated_values_limits_and_features() {
    require_database!();
    let (app, _admin, mailbox_id, _token) = app_with_address().await;
    let (client_token, _refresh, device_id) =
        client_login(&app, "alice@example.net", PASSWORD).await;

    let response = app
        .client_get("/api/v1/client/account", Some(&client_token))
        .await;
    let body = response.expect(StatusCode::OK);
    assert_eq!(body["user"]["email"], "alice@example.net");
    assert_eq!(body["protocol_version"], 1);
    assert_eq!(body["min_protocol_version"], 1);
    assert!(body["server_version"].is_string());
    assert!(body["server_hostname"].is_string());
    assert_eq!(body["limits"]["max_recipients"], 100);
    assert_eq!(body["limits"]["attachment_chunk_size"], 1_048_576);
    assert_eq!(body["limits"]["sync_page_size"], 500);
    let features: Vec<&str> = body["features"]
        .as_array()
        .expect("features")
        .iter()
        .filter_map(|value| value.as_str())
        .collect();
    for expected in ["sync", "events", "drafts", "attachments", "devices", "search"] {
        assert!(features.contains(&expected), "missing {expected}: {features:?}");
    }
    assert_eq!(
        body["mailboxes"][0]["id"].as_i64(),
        Some(mailbox_id)
    );
    let _ = device_id;

    app.cleanup().await;
}

#[tokio::test]
async fn a_client_below_the_protocol_floor_gets_426_with_the_documented_body() {
    require_database!();
    let (app, _admin, _mailbox, _token) = app_with_address().await;

    let request = axum::http::Request::builder()
        .method("GET")
        .uri("/api/v1/client/account")
        .header("x-ferroma-client", "FerromaClient/0.1.0")
        .header("x-ferroma-protocol", "0")
        .body(axum::body::Body::empty())
        .expect("valid request");
    let response = app.request(request).await;

    assert_eq!(response.status, StatusCode::UPGRADE_REQUIRED);
    let body = response.json();
    assert_eq!(body["error"]["code"], "unsupported");
    assert_eq!(
        body["error"]["message"],
        "unsupported: client protocol 0 is no longer supported; upgrade to FCP/1"
    );

    app.cleanup().await;
}

#[tokio::test]
async fn a_missing_protocol_header_is_treated_as_protocol_one() {
    require_database!();
    let (app, _admin, _mailbox, _token) = app_with_address().await;
    let (client_token, _refresh, _device) =
        client_login(&app, "alice@example.net", PASSWORD).await;

    // No `X-Ferroma-Protocol` at all: `curl` and monitoring must still work.
    let request = axum::http::Request::builder()
        .method("GET")
        .uri("/api/v1/client/account")
        .header(axum::http::header::AUTHORIZATION, format!("Bearer {client_token}"))
        .body(axum::body::Body::empty())
        .expect("valid request");
    let response = app.request(request).await;
    response.expect(StatusCode::OK);

    app.cleanup().await;
}

#[tokio::test]
async fn a_client_asking_for_a_higher_protocol_is_served_the_servers_version() {
    require_database!();
    let (app, _admin, _mailbox, _token) = app_with_address().await;
    let (client_token, _refresh, _device) =
        client_login(&app, "alice@example.net", PASSWORD).await;

    let request = axum::http::Request::builder()
        .method("GET")
        .uri("/api/v1/client/mailboxes")
        .header("x-ferroma-protocol", "99")
        .header(axum::http::header::AUTHORIZATION, format!("Bearer {client_token}"))
        .body(axum::body::Body::empty())
        .expect("valid request");
    let response = app.request(request).await;
    response.expect(StatusCode::OK);
    assert_eq!(
        response.header("x-ferroma-protocol").as_deref(),
        Some("1"),
        "the server reports the version it actually used"
    );

    app.cleanup().await;
}

#[tokio::test]
async fn the_client_surface_rejects_a_browser_session_cookie() {
    require_database!();
    let (app, _admin, _mailbox, _token) = app_with_address().await;

    // A cookie-login token is a session secret, not a bearer token.
    let browser = app
        .json(
            "POST",
            "/api/v1/auth/login",
            None,
            json!({ "email": "alice@example.net", "password": PASSWORD }),
        )
        .await;
    let cookie = browser.header("set-cookie").expect("cookie");
    let pair = cookie.split(';').next().expect("name=value").to_string();

    let request = axum::http::Request::builder()
        .method("GET")
        .uri("/api/v1/client/mailboxes")
        .header("x-ferroma-protocol", "1")
        .header(axum::http::header::COOKIE, pair)
        .body(axum::body::Body::empty())
        .expect("valid request");
    let response = app.request(request).await;
    assert_eq!(
        response.status,
        StatusCode::UNAUTHORIZED,
        "the client API is bearer-only"
    );

    app.cleanup().await;
}

#[tokio::test]
async fn the_client_mailbox_list_groups_folders_under_each_address() {
    require_database!();
    let (app, _admin, _mailbox, _token) = app_with_address().await;
    let (client_token, _refresh, _device) =
        client_login(&app, "alice@example.net", PASSWORD).await;

    let body = app
        .client_get("/api/v1/client/mailboxes", Some(&client_token))
        .await
        .expect(StatusCode::OK);
    let mailboxes = body["mailboxes"].as_array().expect("mailboxes");
    assert_eq!(mailboxes.len(), 1);
    assert_eq!(mailboxes[0]["address"], "alice@example.net");
    assert_eq!(mailboxes[0]["is_primary"], true);
    let folders = mailboxes[0]["folders"].as_array().expect("folders");
    let names: Vec<&str> = folders
        .iter()
        .filter_map(|folder| folder["name"].as_str())
        .collect();
    for expected in ["INBOX", "Sent", "Drafts", "Trash", "Junk", "Archive"] {
        assert!(names.contains(&expected), "missing {expected}: {names:?}");
    }
    // The counters and UID state a client maps on are present.
    let inbox = folders
        .iter()
        .find(|folder| folder["name"] == "INBOX")
        .expect("INBOX");
    assert!(inbox["message_count"].is_number());
    assert!(inbox["unseen_count"].is_number());
    assert!(inbox["uid_validity"].is_number());
    assert!(inbox["uid_next"].is_number());

    app.cleanup().await;
}

#[tokio::test]
async fn sync_pages_through_the_change_log_in_order() {
    require_database!();
    let (app, _admin, mailbox_id, token) = app_with_address().await;
    let (client_token, _refresh, _device) =
        client_login(&app, "alice@example.net", PASSWORD).await;

    // Record a few changes: each send adds a `message_created`.
    let mut message_ids = Vec::new();
    for index in 0..3 {
        let sent = app
            .json(
                "POST",
                "/api/v1/messages",
                Some(&token),
                json!({
                    "from": "alice@example.net",
                    "to": ["bob@example.org"],
                    "subject": format!("sync {index}"),
                    "text": "body"
                }),
            )
            .await
        .expect(StatusCode::OK);
        message_ids.push(sent["message_id"].as_i64().expect("message id"));
    }

    // A first sync from cursor 0 sees everything.
    let first = app
        .client_get(
            &format!("/api/v1/client/sync?mailbox_id={mailbox_id}&cursor=0"),
            Some(&client_token),
        )
        .await;
    let body = first.expect(StatusCode::OK);
    let changes = body["changes"].as_array().expect("changes");
    assert_eq!(changes.len(), 3, "{body}");
    assert_eq!(body["has_more"], false);
    let seqs: Vec<i64> = changes
        .iter()
        .filter_map(|change| change["seq"].as_i64())
        .collect();
    let mut sorted = seqs.clone();
    sorted.sort_unstable();
    assert_eq!(seqs, sorted, "changes are ordered by seq ascending");
    for change in changes {
        assert_eq!(change["type"], "message_created");
        assert!(change["message_id"].is_number());
        assert!(change["uid"].is_number());
        // Metadata first: no body in a change.
        assert!(change.get("text_body").is_none(), "{change}");
    }
    let next_cursor = body["next_cursor"]
        .as_str()
        .expect("next_cursor is a string")
        .to_string();
    assert!(body["latest_cursor"].is_string());

    // Nothing new since: an empty page that keeps the cursor.
    let empty = app
        .client_get(
            &format!("/api/v1/client/sync?mailbox_id={mailbox_id}&cursor={next_cursor}"),
            Some(&client_token),
        )
        .await
        .expect(StatusCode::OK);
    assert_eq!(empty["changes"].as_array().map(Vec::len), Some(0));
    assert_eq!(empty["has_more"], false);
    assert_eq!(empty["next_cursor"], next_cursor);

    // One more change, then a paged sync with `limit=1`.
    app.json(
        "POST",
        "/api/v1/messages",
        Some(&token),
        json!({
            "from": "alice@example.net",
            "to": ["bob@example.org"],
            "subject": "fourth",
            "text": "body"
        }),
    )
    .await
        .expect(StatusCode::OK);

    let page = app
        .client_get(
            &format!("/api/v1/client/sync?mailbox_id={mailbox_id}&cursor={next_cursor}&limit=1"),
            Some(&client_token),
        )
        .await
        .expect(StatusCode::OK);
    assert_eq!(page["changes"].as_array().map(Vec::len), Some(1));
    assert_eq!(page["has_more"], false, "the page held the only new change");
    assert_ne!(page["next_cursor"], next_cursor);

    app.cleanup().await;
}

#[tokio::test]
async fn sync_pages_report_has_more_when_more_changes_are_waiting() {
    require_database!();
    let (app, _admin, mailbox_id, token) = app_with_address().await;
    let (client_token, _refresh, _device) =
        client_login(&app, "alice@example.net", PASSWORD).await;

    for index in 0..3 {
        app.json(
            "POST",
            "/api/v1/messages",
            Some(&token),
            json!({
                "from": "alice@example.net",
                "to": ["bob@example.org"],
                "subject": format!("page {index}"),
                "text": "body"
            }),
        )
        .await
        .expect(StatusCode::OK);
    }

    let first = app
        .client_get(
            &format!("/api/v1/client/sync?mailbox_id={mailbox_id}&limit=1"),
            Some(&client_token),
        )
        .await
        .expect(StatusCode::OK);
    assert_eq!(first["changes"].as_array().map(Vec::len), Some(1));
    assert_eq!(first["has_more"], true, "{first}");

    // Walking the pages from the reported cursor reaches the end without gaps.
    let mut cursor = first["next_cursor"]
        .as_str()
        .expect("cursor")
        .to_string();
    let mut seen = 1;
    for _ in 0..10 {
        let page = app
            .client_get(
                &format!("/api/v1/client/sync?mailbox_id={mailbox_id}&cursor={cursor}&limit=1"),
                Some(&client_token),
            )
            .await
        .expect(StatusCode::OK);
        let count = page["changes"].as_array().map(Vec::len).unwrap_or(0);
        seen += count;
        cursor = page["next_cursor"].as_str().unwrap_or("0").to_string();
        if page["has_more"] == false {
            break;
        }
    }
    assert_eq!(seen, 3, "every change was delivered exactly once");

    app.cleanup().await;
}

#[tokio::test]
async fn a_cursor_ahead_of_the_server_is_a_conflict() {
    require_database!();
    let (app, _admin, mailbox_id, _token) = app_with_address().await;
    let (client_token, _refresh, _device) =
        client_login(&app, "alice@example.net", PASSWORD).await;

    let response = app
        .client_get(
            &format!("/api/v1/client/sync?mailbox_id={mailbox_id}&cursor=999999"),
            Some(&client_token),
        )
        .await;
    assert_eq!(response.status, StatusCode::CONFLICT);
    let body = response.json();
    assert_eq!(body["error"]["code"], "conflict");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("resync"),
        "{body}"
    );

    // The documented phrase for a stale cursor is what a client matches on.
    assert!(body["error"]["message"]
        .as_str()
        .unwrap_or_default()
        .contains("full resync required"));

    app.cleanup().await;
}

#[tokio::test]
async fn a_cursor_older_than_the_retained_history_is_a_conflict() {
    require_database!();
    let (app, _admin, mailbox_id, token) = app_with_address().await;
    let (client_token, _refresh, _device) =
        client_login(&app, "alice@example.net", PASSWORD).await;

    for index in 0..3 {
        app.json(
            "POST",
            "/api/v1/messages",
            Some(&token),
            json!({
                "from": "alice@example.net",
                "to": ["bob@example.org"],
                "subject": format!("history {index}"),
                "text": "body"
            }),
        )
        .await
        .expect(StatusCode::OK);
    }

    // Delete the oldest change-log rows, which is what retention does in production.
    app.db()
        .execute("DELETE FROM change_log WHERE seq = (SELECT MIN(seq) FROM change_log)")
        .await
        .expect("the delete must apply");
    app.db()
        .execute("DELETE FROM change_log WHERE seq = (SELECT MIN(seq) FROM change_log)")
        .await
        .expect("the delete must apply");

    let response = app
        .client_get(
            &format!("/api/v1/client/sync?mailbox_id={mailbox_id}&cursor=1"),
            Some(&client_token),
        )
        .await;
    assert_eq!(response.status, StatusCode::CONFLICT);
    assert_eq!(response.error_code(), "conflict");
    assert!(response
        .text()
        .contains("cursor too old; full resync required"), "{}", response.text());

    app.cleanup().await;
}

#[tokio::test]
async fn sync_refuses_a_mailbox_that_belongs_to_somebody_else() {
    require_database!();
    let (app, admin, _mailbox, _token) = app_with_address().await;
    let (_bob_id, bob_mailbox, _bob) =
        seed_account(&app, &admin, "example.net", "bob@example.net", PASSWORD).await;
    let (client_token, _refresh, _device) =
        client_login(&app, "alice@example.net", PASSWORD).await;

    let response = app
        .client_get(
            &format!("/api/v1/client/sync?mailbox_id={bob_mailbox}"),
            Some(&client_token),
        )
        .await;
    assert_eq!(response.status, StatusCode::NOT_FOUND);
    assert_eq!(response.error_code(), "not_found");

    app.cleanup().await;
}

#[tokio::test]
async fn sync_requires_a_mailbox_id() {
    require_database!();
    let (app, _admin, _mailbox, _token) = app_with_address().await;
    let (client_token, _refresh, _device) =
        client_login(&app, "alice@example.net", PASSWORD).await;

    let response = app
        .client_get("/api/v1/client/sync", Some(&client_token))
        .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert_eq!(response.error_code(), "invalid_input");

    app.cleanup().await;
}

#[tokio::test]
async fn the_same_operation_id_twice_has_one_side_effect() {
    require_database!();
    let (app, _admin, mailbox_id, token) = app_with_address().await;
    let (client_token, _refresh, _device) =
        client_login(&app, "alice@example.net", PASSWORD).await;

    // Send through the client surface with an operation id.
    let send = json!({
        "from": "alice@example.net",
        "to": ["bob@example.org"],
        "subject": "idempotent send",
        "text": "body",
        "operation_id": "op_send_once"
    });
    let first = app
        .client_json(
            "POST",
            "/api/v1/client/messages?operation_id=op_send_once",
            Some(&client_token),
            send.clone(),
        )
        .await;
    let first_body = first.expect(StatusCode::OK);
    let message_id = first_body["message_id"].as_i64().expect("message id");
    assert_eq!(first_body["queued"], 1);

    // The retry replays the recorded response instead of sending again.
    let second = app
        .client_json(
            "POST",
            "/api/v1/client/messages?operation_id=op_send_once",
            Some(&client_token),
            send,
        )
        .await;
    let second_body = second.expect(StatusCode::OK);
    assert_eq!(second_body["message_id"].as_i64(), Some(message_id));

    // One message, one queue row: the side effect happened once.
    assert_eq!(app.db().count("messages").await, 1);
    assert_eq!(app.db().count("mail_queue").await, 1);
    assert_eq!(app.db().count("operations").await, 1);

    // A *different* id does send again.
    let third = app
        .client_json(
            "POST",
            "/api/v1/client/messages?operation_id=op_send_twice",
            Some(&client_token),
            json!({
                "from": "alice@example.net",
                "to": ["bob@example.org"],
                "subject": "second send",
                "text": "body"
            }),
        )
        .await;
    third.expect(StatusCode::OK);
    assert_eq!(app.db().count("messages").await, 2);

    let _ = (mailbox_id, token);

    app.cleanup().await;
}

#[tokio::test]
async fn a_replayed_operation_id_on_a_move_does_not_move_twice() {
    require_database!();
    let (app, _admin, mailbox_id, token) = app_with_address().await;
    let (client_token, _refresh, _device) =
        client_login(&app, "alice@example.net", PASSWORD).await;

    let sent = app
        .json(
            "POST",
            "/api/v1/messages",
            Some(&token),
            json!({
                "from": "alice@example.net",
                "to": ["bob@example.org"],
                "subject": "move once",
                "text": "body"
            }),
        )
        .await
        .expect(StatusCode::OK);
    let message_id = sent["message_id"].as_i64().expect("message id");

    let archive = folder_id(&app, &token, mailbox_id, "Archive").await;
    let move_body = json!({ "folder_id": archive, "operation_id": "op_move_once" });

    let first = app
        .client_json(
            "POST",
            &format!("/api/v1/client/messages/{message_id}/move"),
            Some(&client_token),
            move_body.clone(),
        )
        .await;
    first.expect(StatusCode::OK);

    let second = app
        .client_json(
            "POST",
            &format!("/api/v1/client/messages/{message_id}/move"),
            Some(&client_token),
            move_body,
        )
        .await;
    second.expect(StatusCode::OK);

    // Still exactly one row, still in Archive, and its UID was allocated once.
    assert_eq!(app.db().count("messages").await, 1);
    let folder = app
        .get(&format!("/api/v1/messages/{message_id}"), Some(&token))
        .await
        .expect(StatusCode::OK);
    assert_eq!(folder["folder_id"].as_i64(), Some(archive));
    assert_eq!(
        app.db().count("operations").await,
        1,
        "only the explicitly identified operation is recorded"
    );

    app.cleanup().await;
}

#[tokio::test]
async fn the_client_message_verbs_map_onto_the_same_state_as_the_management_ones() {
    require_database!();
    let (app, _admin, mailbox_id, token) = app_with_address().await;
    let (client_token, _refresh, _device) =
        client_login(&app, "alice@example.net", PASSWORD).await;

    let sent = app
        .json(
            "POST",
            "/api/v1/messages",
            Some(&token),
            json!({
                "from": "alice@example.net",
                "to": ["bob@example.org"],
                "subject": "client verbs",
                "text": "body"
            }),
        )
        .await
        .expect(StatusCode::OK);
    let message_id = sent["message_id"].as_i64().expect("message id");

    // `unread` clears `\Seen`...
    let unread = app
        .client_json(
            "POST",
            &format!("/api/v1/client/messages/{message_id}/unread"),
            Some(&client_token),
            json!({}),
        )
        .await;
    let body = unread.expect(StatusCode::OK);
    assert!(!body["flags"].as_str().unwrap_or_default().contains("seen"));

    // ...and `read` sets it again.
    let read = app
        .client_json(
            "POST",
            &format!("/api/v1/client/messages/{message_id}/read"),
            Some(&client_token),
            json!({}),
        )
        .await;
    let body = read.expect(StatusCode::OK);
    assert!(body["flags"].as_str().unwrap_or_default().contains("seen"));

    // `star` sets `\Flagged`.
    let starred = app
        .client_json(
            "POST",
            &format!("/api/v1/client/messages/{message_id}/star"),
            Some(&client_token),
            json!({}),
        )
        .await;
    let body = starred.expect(StatusCode::OK);
    assert!(body["flags"].as_str().unwrap_or_default().contains("flagged"));

    // `archive` moves it out of Sent.
    let archived = app
        .client_json(
            "POST",
            &format!("/api/v1/client/messages/{message_id}/archive"),
            Some(&client_token),
            json!({}),
        )
        .await;
    let body = archived.expect(StatusCode::OK);
    let archive = folder_id(&app, &token, mailbox_id, "Archive").await;
    assert_eq!(body["folder_id"].as_i64(), Some(archive));

    // `trash` moves it to Trash.
    let trashed = app
        .client_json(
            "POST",
            &format!("/api/v1/client/messages/{message_id}/trash"),
            Some(&client_token),
            json!({}),
        )
        .await;
    assert_eq!(trashed.status, StatusCode::NO_CONTENT);
    let trash = folder_id(&app, &token, mailbox_id, "Trash").await;
    let listed = app
        .client_get(
            &format!("/api/v1/client/messages?folder_id={trash}"),
            Some(&client_token),
        )
        .await
        .expect(StatusCode::OK);
    assert_eq!(listed["total"], 1, "{listed}");

    app.cleanup().await;
}

#[tokio::test]
async fn a_client_cannot_reach_another_users_message() {
    require_database!();
    let (app, admin, _mailbox, token) = app_with_address().await;
    // Bob's own client login: the point of this test is that *his* credential cannot
    // reach Alice's message.
    let (_bob_id, _bob_mailbox, _bob_management) =
        seed_account(&app, &admin, "example.net", "bob@example.net", PASSWORD).await;
    let (_bob_token, _bob_refresh, _bob_device) = client_login(&app, "bob@example.net", PASSWORD).await;
    let (client_token, _refresh, _device) =
        client_login(&app, "alice@example.net", PASSWORD).await;

    let sent = app
        .json(
            "POST",
            "/api/v1/messages",
            Some(&token),
            json!({
                "from": "alice@example.net",
                "to": ["bob@example.org"],
                "subject": "private",
                "text": "body"
            }),
        )
        .await
        .expect(StatusCode::OK);
    let message_id = sent["message_id"].as_i64().expect("message id");

    // Alice's own client can read it...
    app.client_get(&format!("/api/v1/client/messages/{message_id}"), Some(&client_token))
        .await
        .expect(StatusCode::OK);

    // ...and Bob's cannot, on any verb.
    assert_eq!(
        app.client_get(&format!("/api/v1/client/messages/{message_id}"), Some(&_bob_token))
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        app.client_get(
            &format!("/api/v1/client/messages/{message_id}/raw"),
            Some(&_bob_token)
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );

    assert_eq!(
        app.client_json(
            "PATCH",
            &format!("/api/v1/client/messages/{message_id}"),
            Some(&_bob_token),
            json!({ "seen": true })
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        app.client_json(
            "POST",
            &format!("/api/v1/client/messages/{message_id}/read"),
            Some(&_bob_token),
            json!({})
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        app.client_json(
            "POST",
            &format!("/api/v1/client/messages/{message_id}/move"),
            Some(&_bob_token),
            json!({ "folder_id": _bob_mailbox })
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );

    // Alice's message is untouched: a sent copy carries `seen` and nothing more.
    let still = app
        .client_get(&format!("/api/v1/client/messages/{message_id}"), Some(&client_token))
        .await
        .expect(StatusCode::OK);
    assert_eq!(
        still["flags"].as_str().unwrap_or_default(),
        "seen",
        "Bob's attempt must not have changed Alice's message"
    );

    app.cleanup().await;
}

#[tokio::test]
async fn client_drafts_are_the_same_records_as_the_management_surface() {
    require_database!();
    let (app, _admin, mailbox_id, token) = app_with_address().await;
    let (client_token, _refresh, _device) =
        client_login(&app, "alice@example.net", PASSWORD).await;

    let created = app
        .client_json(
            "POST",
            "/api/v1/client/drafts",
            Some(&client_token),
            json!({
                "mailbox_id": mailbox_id,
                "subject": "from the client",
                "text": "draft body",
                "to": ["bob@example.org"]
            }),
        )
        .await;
    let body = created.expect(StatusCode::CREATED);
    let draft_id = body["id"].as_i64().expect("draft id");

    // The management surface sees the same record.
    let listed = app
        .get("/api/v1/drafts", Some(&token))
        .await
        .expect(StatusCode::OK);
    assert_eq!(listed["total"], 1);
    assert_eq!(listed["items"][0]["id"].as_i64(), Some(draft_id));
    assert_eq!(listed["items"][0]["subject"], "from the client");

    let patched = app
        .client_json(
            "PATCH",
            &format!("/api/v1/client/drafts/{draft_id}"),
            Some(&client_token),
            json!({ "text": "edited by the client" }),
        )
        .await
        .expect(StatusCode::OK);
    assert_eq!(patched["text"], "edited by the client");

    // And a management read sees the edit.
    let fetched = app
        .get(&format!("/api/v1/drafts/{draft_id}"), Some(&token))
        .await
        .expect(StatusCode::OK);
    assert_eq!(fetched["text"], "edited by the client");

    app.cleanup().await;
}

#[tokio::test]
async fn device_register_list_revoke_makes_the_token_stop_working() {
    require_database!();
    let (app, _admin, _mailbox, _token) = app_with_address().await;
    let (client_token, _refresh, device_id) =
        client_login(&app, "alice@example.net", PASSWORD).await;

    // The device shows up in the client's own list.
    let listed = app
        .client_get("/api/v1/client/devices", Some(&client_token))
        .await
        .expect(StatusCode::OK);
    let devices = listed["devices"].as_array().expect("devices");
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0]["id"].as_i64(), Some(device_id));
    assert_eq!(devices[0]["device_uid"], "install-3f2c");
    assert_eq!(devices[0]["platform"], "windows");
    assert_eq!(devices[0]["client_version"], "0.7.0");
    assert_eq!(devices[0]["protocol_version"], 1);
    assert_eq!(devices[0]["revoked"], false);

    // Revoke it.
    let revoked = app
        .client_json(
            "POST",
            &format!("/api/v1/client/devices/{device_id}/revoke"),
            Some(&client_token),
            json!({}),
        )
        .await;
    let body = revoked.expect(StatusCode::OK);
    assert_eq!(body["revoked"], true);

    // The device's token is refused from now on, on every endpoint.
    for path in [
        "/api/v1/client/account",
        "/api/v1/client/mailboxes",
        "/api/v1/client/devices",
    ] {
        let response = app.client_get(path, Some(&client_token)).await;
        assert_eq!(
            response.status,
            StatusCode::UNAUTHORIZED,
            "{path} accepted a revoked device's token"
        );
        assert_eq!(response.error_code(), "unauthorized");
    }

    // The sessions belonging to it were revoked too. The management login the fixture
    // performed is a *different* session, so only the device's own must be gone.
    let device_id: i64 = app
        .db()
        .scalar::<i64>("SELECT id::BIGINT FROM devices WHERE device_uid = 'install-3f2c'")
        .await
        .unwrap_or(0);
    let device_sessions: i64 = app
        .db()
        .scalar::<i64>(&format!(
            "SELECT COUNT(*)::BIGINT FROM sessions
              WHERE device_id = {device_id} AND revoked_at IS NULL"
        ))
        .await
        .unwrap_or(0);
    assert_eq!(
        device_sessions, 0,
        "the device's session must be revoked with it"
    );

    app.cleanup().await;
}

#[tokio::test]
async fn deleting_a_device_has_the_same_effect_as_revoking_it() {
    require_database!();
    let (app, _admin, _mailbox, _token) = app_with_address().await;
    let (client_token, _refresh, device_id) =
        client_login(&app, "alice@example.net", PASSWORD).await;

    let deleted = app
        .client_json(
            "DELETE",
            &format!("/api/v1/client/devices/{device_id}"),
            Some(&client_token),
            json!({}),
        )
        .await;
    assert_eq!(deleted.status, StatusCode::OK);

    assert_eq!(
        app.client_get("/api/v1/client/account", Some(&client_token))
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );

    app.cleanup().await;
}

#[tokio::test]
async fn one_user_cannot_revoke_another_users_device() {
    require_database!();
    let (app, admin, _mailbox, _token) = app_with_address().await;
    let (_bob_id, _bob_mailbox, _bob) =
        seed_account(&app, &admin, "example.net", "bob@example.net", PASSWORD).await;
    let (_client_token, _refresh, device_id) =
        client_login(&app, "alice@example.net", PASSWORD).await;

    let bob_client = app
        .client_json(
            "POST",
            "/api/v1/client/auth/login",
            None,
            json!({
                "email": "bob@example.net",
                "password": PASSWORD,
                "device": { "device_uid": "bob-install", "name": "Bob's phone" }
            }),
        )
        .await
        .expect(StatusCode::OK);
    let bob_token = bob_client["access_token"]
        .as_str()
        .expect("bob token")
        .to_string();

    let response = app
        .client_json(
            "POST",
            &format!("/api/v1/client/devices/{device_id}/revoke"),
            Some(&bob_token),
            json!({}),
        )
        .await;
    assert_eq!(response.status, StatusCode::NOT_FOUND);
    assert_eq!(response.error_code(), "not_found");

    app.cleanup().await;
}

#[tokio::test]
async fn the_admin_device_list_resolves_the_owner_and_can_revoke() {
    require_database!();
    let (app, admin, _mailbox, _token) = app_with_address().await;
    let (_client_token, _refresh, device_id) =
        client_login(&app, "alice@example.net", PASSWORD).await;

    let listed = app
        .get("/api/v1/devices", Some(&admin))
        .await
        .expect(StatusCode::OK);
    assert_eq!(listed["total"], 1);
    assert_eq!(listed["items"][0]["email"], "alice@example.net");
    assert_eq!(listed["items"][0]["revoked"], false);
    assert_eq!(listed["items"][0]["last_ip"], serde_json::Value::Null);

    let filtered = app
        .get("/api/v1/devices?platform=windows", Some(&admin))
        .await
        .expect(StatusCode::OK);
    assert_eq!(filtered["total"], 1);
    let none = app
        .get("/api/v1/devices?platform=ios", Some(&admin))
        .await
        .expect(StatusCode::OK);
    assert_eq!(none["total"], 0);

    let revoked = app
        .json(
            "POST",
            &format!("/api/v1/devices/{device_id}/revoke"),
            Some(&admin),
            json!({}),
        )
        .await;
    let body = revoked.expect(StatusCode::OK);
    assert_eq!(body["revoked"], true);

    // Revoked devices are hidden unless asked for.
    let visible = app
        .get("/api/v1/devices", Some(&admin))
        .await
        .expect(StatusCode::OK);
    assert_eq!(visible["total"], 0);
    let with_revoked = app
        .get("/api/v1/devices?include_revoked=true", Some(&admin))
        .await
        .expect(StatusCode::OK);
    assert_eq!(with_revoked["total"], 1);

    app.cleanup().await;
}

#[tokio::test]
async fn the_client_search_fallback_understands_the_documented_operators() {
    require_database!();
    let (app, _admin, mailbox_id, token) = app_with_address().await;
    let (client_token, _refresh, _device) =
        client_login(&app, "alice@example.net", PASSWORD).await;

    for (subject, recipient) in [
        ("quarterly invoice", "billing@example.org"),
        ("team standup", "team@example.org"),
    ] {
        app.json(
            "POST",
            "/api/v1/messages",
            Some(&token),
            json!({
                "from": "alice@example.net",
                "to": [recipient],
                "subject": subject,
                "text": "body"
            }),
        )
        .await
        .expect(StatusCode::OK);
    }

    let by_subject = app
        .client_get(
            &format!("/api/v1/client/search?q=subject:invoice&mailbox_id={mailbox_id}"),
            Some(&client_token),
        )
        .await
        .expect(StatusCode::OK);
    assert_eq!(by_subject["total"], 1, "{by_subject}");
    assert_eq!(by_subject["items"][0]["subject"], "quarterly invoice");

    let free_text = app
        .client_get(
            &format!("/api/v1/client/search?q=standup&mailbox_id={mailbox_id}"),
            Some(&client_token),
        )
        .await
        .expect(StatusCode::OK);
    assert_eq!(free_text["total"], 1);

    let nothing = app
        .client_get(
            &format!("/api/v1/client/search?q=nothingmatches&mailbox_id={mailbox_id}"),
            Some(&client_token),
        )
        .await
        .expect(StatusCode::OK);
    assert_eq!(nothing["total"], 0);

    app.cleanup().await;
}

#[tokio::test]
async fn the_events_route_exists_and_authenticates_before_it_upgrades() {
    require_database!();
    let (app, _admin, _mailbox, _token) = app_with_address().await;
    let (client_token, _refresh, _device) =
        client_login(&app, "alice@example.net", PASSWORD).await;

    // `tokio-tungstenite` is not a dependency of this crate and the brief forbids adding
    // one, so the socket is tested at the HTTP layer plus the frame builder's unit
    // tests. Without credentials the route must refuse before anything is upgraded.
    let unauthorised = app.client_get("/api/v1/client/events", None).await;
    assert_eq!(
        unauthorised.status,
        StatusCode::UNAUTHORIZED,
        "the socket authenticates before it upgrades"
    );
    assert_eq!(unauthorised.error_code(), "unauthorized");

    // With a credential, the route is found and the upgrade extractor is what answers —
    // which is what proves the endpoint is registered rather than missing.
    let plain = app
        .client_get("/api/v1/client/events?cursor=7", Some(&client_token))
        .await;
    assert_ne!(
        plain.status,
        StatusCode::NOT_FOUND,
        "the events route must exist: {}",
        plain.text()
    );
    assert!(
        plain.status.is_client_error(),
        "a non-upgrade request is the caller's mistake: {}",
        plain.text()
    );

    // A client below the protocol floor is refused with 426, before the upgrade.
    let request = axum::http::Request::builder()
        .method("GET")
        .uri("/api/v1/client/events")
        .header("x-ferroma-protocol", "0")
        .header(axum::http::header::AUTHORIZATION, format!("Bearer {client_token}"))
        .header(axum::http::header::CONNECTION, "upgrade")
        .header(axum::http::header::UPGRADE, "websocket")
        .header("sec-websocket-version", "13")
        .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
        .body(axum::body::Body::empty())
        .expect("valid request");
    let refused = app.request(request).await;
    assert_eq!(refused.status, StatusCode::UPGRADE_REQUIRED);
    assert_eq!(refused.error_code(), "unsupported");

    app.cleanup().await;
}

#[tokio::test]
async fn the_attachment_chunk_protocol_resumes_and_verifies_the_digest() {
    require_database!();
    // A 1000-byte chunk size makes the geometry easy to reason about: a 2500-byte
    // upload is three chunks, and the last one is short.
    let mut config = ferroma_core::Config::default();
    config.client.attachment_chunk_size = 1000;
    let app = TestApp::with_config(config).await;
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
    seed_account(&app, &admin, "example.net", "alice@example.net", PASSWORD).await;
    let (client_token, _refresh, _device) =
        client_login(&app, "alice@example.net", PASSWORD).await;

    let payload: Vec<u8> = (0..2500u32).map(|value| (value % 251) as u8).collect();
    let digest = {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(&payload);
        format!("{:x}", hasher.finalize())
    };

    let init = app
        .client_json(
            "POST",
            "/api/v1/client/attachments/init",
            Some(&client_token),
            json!({
                "filename": "big.bin",
                "content_type": "application/octet-stream",
                "size_bytes": payload.len()
            }),
        )
        .await;
    let body = init.expect(StatusCode::CREATED);
    let attachment_id = body["attachment_id"].as_i64().expect("attachment id");
    let chunk_size = body["chunk_size"].as_u64().expect("chunk size");
    assert_eq!(chunk_size, 1000);
    assert!(body["upload_token"]
        .as_str()
        .unwrap_or_default()
        .starts_with("up_"));

    // Send the chunks out of order, then re-send one (chunks are idempotent per index).
    let chunks: Vec<Vec<u8>> = payload.chunks(1000).map(<[u8]>::to_vec).collect();
    assert_eq!(chunks.len(), 3);
    for index in [1u64, 0u64, 0u64] {
        let request = axum::http::Request::builder()
            .method("PUT")
            .uri(format!(
                "/api/v1/client/attachments/{attachment_id}/chunk?index={index}"
            ))
            .header(axum::http::header::AUTHORIZATION, format!("Bearer {client_token}"))
            .header(axum::http::header::CONTENT_TYPE, "application/octet-stream")
            .body(axum::body::Body::from(chunks[index as usize].clone()))
            .expect("valid request");
        let response = app.request(request).await;
        assert_eq!(response.status, StatusCode::NO_CONTENT, "chunk {index}");
    }

    // The status reports exactly the gap a crashed client would have to resend.
    // The status reports the exact gap a crashed client would have to resend. It is
    // asked *now*, because a successful `complete` replaces the reserved row.
    let status = app
        .client_get(
            &format!("/api/v1/client/attachments/{attachment_id}/status"),
            Some(&client_token),
        )
        .await
        .expect(StatusCode::OK);
    // MARK-A
    assert_eq!(status["size_bytes"], 2500, "{status}");
    assert_eq!(status["chunk_count"], 3, "{status}");
    assert_eq!(status["received"], json!([0, 1]));
    assert_eq!(status["complete"], false, "{status}");

    // Completing with a gap is refused and the session stays resumable.
    let incomplete = app
        .client_json(
            "POST",
            &format!("/api/v1/client/attachments/{attachment_id}/complete"),
            Some(&client_token),
            json!({ "sha256": digest }),
        )
        .await;
    assert_eq!(incomplete.status, StatusCode::CONFLICT);

    // Send the last chunk and finish.
    let request = axum::http::Request::builder()
        .method("PUT")
        .uri(format!(
            "/api/v1/client/attachments/{attachment_id}/chunk?index=2"
        ))
        .header(axum::http::header::AUTHORIZATION, format!("Bearer {client_token}"))
        .body(axum::body::Body::from(chunks[2].clone()))
        .expect("valid request");
    app.request(request).await.is(StatusCode::NO_CONTENT);

    let complete = app
        .client_json(
            "POST",
            &format!("/api/v1/client/attachments/{attachment_id}/complete"),
            Some(&client_token),
            json!({ "sha256": digest }),
        )
        .await;
    let body = complete.expect(StatusCode::OK);
    assert_eq!(body["size_bytes"].as_u64(), Some(payload.len() as u64));
    assert_eq!(body["sha256"], digest);

    // The assembled bytes are exactly what was uploaded.
    let download = app
        .client_get(
            &format!(
                "/api/v1/client/attachments/{}",
                body["id"].as_i64().unwrap_or(0)
            ),
            Some(&client_token),
        )
        .await;
    assert_eq!(download.status, StatusCode::OK);
    assert_eq!(download.body, payload);

    app.cleanup().await;
}

#[tokio::test]
async fn a_chunked_upload_with_the_wrong_digest_is_a_conflict() {
    require_database!();
    let (app, _admin, _mailbox, _token) = app_with_address().await;
    let (client_token, _refresh, _device) =
        client_login(&app, "alice@example.net", PASSWORD).await;

    let payload = b"hello world".to_vec();
    let init = app
        .client_json(
            "POST",
            "/api/v1/client/attachments/init",
            Some(&client_token),
            json!({
                "filename": "small.bin",
                "size_bytes": payload.len()
            }),
        )
        .await
        .expect(StatusCode::CREATED);
    let attachment_id = init["attachment_id"].as_i64().expect("attachment id");

    let request = axum::http::Request::builder()
        .method("PUT")
        .uri(format!(
            "/api/v1/client/attachments/{attachment_id}/chunk?index=0"
        ))
        .header(axum::http::header::AUTHORIZATION, format!("Bearer {client_token}"))
        .body(axum::body::Body::from(payload))
        .expect("valid request");
    app.request(request).await;

    let wrong = app
        .client_json(
            "POST",
            &format!("/api/v1/client/attachments/{attachment_id}/complete"),
            Some(&client_token),
            json!({ "sha256": "0".repeat(64) }),
        )
        .await;
    assert_eq!(wrong.status, StatusCode::CONFLICT);
    assert_eq!(wrong.error_code(), "conflict");

    app.cleanup().await;
}

#[tokio::test]
async fn a_chunked_upload_reports_the_gap_a_crashed_client_must_resend() {
    require_database!();
    let (app, _admin, _mailbox, _token) = app_with_address().await;
    let (client_token, _refresh, _device) =
        client_login(&app, "alice@example.net", PASSWORD).await;

    // Two chunks' worth, with only the first sent.
    let init = app
        .client_json(
            "POST",
            "/api/v1/client/attachments/init",
            Some(&client_token),
            json!({ "filename": "gap.bin", "size_bytes": 2_000_000 }),
        )
        .await
        .expect(StatusCode::CREATED);
    let attachment_id = init["attachment_id"].as_i64().expect("attachment id");

    let request = axum::http::Request::builder()
        .method("PUT")
        .uri(format!(
            "/api/v1/client/attachments/{attachment_id}/chunk?index=0"
        ))
        .header(axum::http::header::AUTHORIZATION, format!("Bearer {client_token}"))
        .body(axum::body::Body::from(vec![7u8; 1_048_576]))
        .expect("valid request");
    app.request(request).await.is(StatusCode::NO_CONTENT);

    let status_response = app
        .client_get(
            &format!("/api/v1/client/attachments/{attachment_id}/status"),
            Some(&client_token),
        )
        .await;
    assert_eq!(
        status_response.status,
        StatusCode::OK,
        "the status probe returned {} with body {:?}",
        status_response.status,
        status_response.text()
    );
    let status = status_response.json();
    assert_eq!(status["chunk_count"], 2);
    assert_eq!(status["received"], json!([0]));
    assert_eq!(status["complete"], false);

    // Completing with a gap is refused, and the session stays resumable.
    let incomplete = app
        .client_json(
            "POST",
            &format!("/api/v1/client/attachments/{attachment_id}/complete"),
            Some(&client_token),
            json!({ "sha256": "0".repeat(64) }),
        )
        .await;
    assert_eq!(incomplete.status, StatusCode::CONFLICT);

    let still = app
        .client_get(
            &format!("/api/v1/client/attachments/{attachment_id}/status"),
            Some(&client_token),
        )
        .await
        .expect(StatusCode::OK);
    // MARK-C
    assert_eq!(still["received"], json!([0]), "the session is still resumable");

    app.cleanup().await;
}

#[tokio::test]
async fn a_chunk_beyond_the_declared_size_is_refused() {
    require_database!();
    let (app, _admin, _mailbox, _token) = app_with_address().await;
    let (client_token, _refresh, _device) =
        client_login(&app, "alice@example.net", PASSWORD).await;

    let init = app
        .client_json(
            "POST",
            "/api/v1/client/attachments/init",
            Some(&client_token),
            json!({ "filename": "tiny.bin", "size_bytes": 10 }),
        )
        .await
        .expect(StatusCode::CREATED);
    let attachment_id = init["attachment_id"].as_i64().expect("attachment id");

    let request = axum::http::Request::builder()
        .method("PUT")
        .uri(format!(
            "/api/v1/client/attachments/{attachment_id}/chunk?index=5"
        ))
        .header(axum::http::header::AUTHORIZATION, format!("Bearer {client_token}"))
        .body(axum::body::Body::from(b"1234567890".to_vec()))
        .expect("valid request");
    let response = app.request(request).await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert_eq!(response.error_code(), "invalid_input");

    app.cleanup().await;
}

#[tokio::test]
async fn a_client_can_send_a_message_and_see_it_in_its_sync_stream() {
    require_database!();
    let (app, _admin, mailbox_id, _token) = app_with_address().await;
    let (client_token, _refresh, _device) =
        client_login(&app, "alice@example.net", PASSWORD).await;

    let sent = app
        .client_json(
            "POST",
            "/api/v1/client/messages",
            Some(&client_token),
            json!({
                "to": ["bob@example.org"],
                "subject": "from the client",
                "text": "hello"
            }),
        )
        .await;
    let body = sent.expect(StatusCode::OK);
    assert_eq!(body["queued"], 1);

    // The `from` address was filled in from the caller's primary address.
    let message_id = body["message_id"].as_i64().expect("message id");
    let detail = app
        .client_get(
            &format!("/api/v1/client/messages/{message_id}"),
            Some(&client_token),
        )
        .await
        .expect(StatusCode::OK);
    assert_eq!(detail["from"]["address"], "alice@example.net");

    // The change is visible through sync, which is the source of truth.
    let sync = app
        .client_get(
            &format!("/api/v1/client/sync?mailbox_id={mailbox_id}"),
            Some(&client_token),
        )
        .await
        .expect(StatusCode::OK);
    assert!(sync["changes"].as_array().map(Vec::len).unwrap_or(0) >= 1);

    app.cleanup().await;
}

#[tokio::test]
async fn logging_a_client_out_revokes_its_session() {
    require_database!();
    let (app, _admin, _mailbox, _token) = app_with_address().await;
    let (client_token, _refresh, _device) =
        client_login(&app, "alice@example.net", PASSWORD).await;

    let logout = app
        .client_json(
            "POST",
            "/api/v1/client/auth/logout",
            Some(&client_token),
            json!({}),
        )
        .await;
    assert_eq!(logout.status, StatusCode::NO_CONTENT);

    assert_eq!(
        app.client_get("/api/v1/client/account", Some(&client_token))
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );

    app.cleanup().await;
}

#[tokio::test]
async fn refreshing_a_client_token_keeps_the_device_association() {
    require_database!();
    let (app, _admin, _mailbox, _token) = app_with_address().await;
    let (_client_token, refresh, device_id) =
        client_login(&app, "alice@example.net", PASSWORD).await;

    let rotated = app
        .client_json(
            "POST",
            "/api/v1/client/auth/refresh",
            None,
            json!({ "refresh_token": refresh, "device_uid": "install-3f2c" }),
        )
        .await;
    let body = rotated.expect(StatusCode::OK);
    assert_eq!(body["device_id"].as_i64(), Some(device_id));

    // A rotation claiming a different installation is refused.
    let wrong_device = app
        .client_json(
            "POST",
            "/api/v1/client/auth/refresh",
            None,
            json!({
                "refresh_token": body["refresh_token"].as_str().unwrap_or_default(),
                "device_uid": "somebody-elses-install"
            }),
        )
        .await;
    assert_eq!(wrong_device.status, StatusCode::UNAUTHORIZED);

    app.cleanup().await;
}

#[tokio::test]
async fn the_login_helpers_agree_with_the_management_surface() {
    require_database!();
    let (app, _admin, _mailbox, _token) = app_with_address().await;

    // The management login still works for the same account the client uses.
    let token = login(&app, "alice@example.net", PASSWORD).await;
    app.get("/api/v1/auth/me", Some(&token))
        .await
        .expect(StatusCode::OK);

    app.cleanup().await;
}