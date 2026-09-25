//! JMAP session discovery and mail submission.
//!
//! A client such as Flectar Mail reads `urn:ietf:params:jmap:submission` from the
//! Session before it will open the mailbox. These tests drive the real router, so
//! a missing capability or a submission that never reaches the queue fails here.

mod common;

use axum::http::{header, StatusCode};
use common::{folder_id, seed_account, TestApp};
use serde_json::json;

const PASSWORD: &str = "correct horse battery";

async fn jmap_token(app: &TestApp, email: &str) -> String {
    app.json(
        "POST",
        "/api/jmap/auth/token",
        None,
        json!({
            "email": email,
            "password": PASSWORD,
            "device_name": "jmap-test"
        }),
    )
    .await
    .expect(StatusCode::OK)["access_token"]
        .as_str()
        .expect("a JMAP token")
        .to_string()
}

async fn bootstrap_admin(app: &TestApp) -> String {
    let response = app
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
        .await;
    response.expect(StatusCode::CREATED)["access_token"]
        .as_str()
        .expect("setup returns a token")
        .to_string()
}
const SUBMISSION: &str = "urn:ietf:params:jmap:submission";
const MAIL: &str = "urn:ietf:params:jmap:mail";
const CORE: &str = "urn:ietf:params:jmap:core";

#[tokio::test]
async fn the_session_advertises_email_submission_for_the_account() {
    require_database!();
    let app = TestApp::new().await;
    let admin = bootstrap_admin(&app).await;
    let (user_id, _mailbox, _management) =
        seed_account(&app, &admin, "example.net", "alice@example.net", PASSWORD).await;
    let token = jmap_token(&app, "alice@example.net").await;

    let response = app.get("/.well-known/jmap", Some(&token)).await;
    let body = response.expect(StatusCode::OK);
    let account = format!("u{user_id}");

    assert!(
        body["capabilities"].get(SUBMISSION).is_some(),
        "the session must advertise submission: {body}"
    );
    assert_eq!(body["capabilities"][SUBMISSION], json!({}));
    assert_eq!(body["primaryAccounts"][SUBMISSION], account);
    assert_eq!(
        body["accounts"][&account]["accountCapabilities"][SUBMISSION]["maxDelayedSend"],
        0
    );
    assert!(body["accounts"][&account]["accountCapabilities"]
        .get(MAIL)
        .is_some());

    app.cleanup().await;
}

#[tokio::test]
async fn submitting_a_stored_email_queues_the_recipient() {
    require_database!();
    let app = TestApp::new().await;
    let admin = bootstrap_admin(&app).await;
    let (user_id, mailbox_id, management) =
        seed_account(&app, &admin, "example.net", "alice@example.net", PASSWORD).await;
    let token = jmap_token(&app, "alice@example.net").await;
    let drafts = folder_id(&app, &management, mailbox_id, "Drafts").await;
    let account = format!("u{user_id}");

    let identity_response = app
        .json(
            "POST",
            "/api/jmap/",
            Some(&token),
            json!({
                "using": [CORE, MAIL, SUBMISSION],
                "methodCalls": [["Identity/get", {"accountId": account}, "i"]]
            }),
        )
        .await;
    assert_eq!(
        identity_response.status,
        StatusCode::OK,
        "identity call failed: {}",
        identity_response.text()
    );
    let identities = identity_response.json();
    let identity = &identities["methodResponses"][0][1]["list"][0];
    assert_eq!(identity["email"], "alice@example.net");
    let identity_id = identity["id"].as_str().expect("identity id");

    let raw = b"From: alice@example.net\r\nTo: bob@example.org\r\nBcc: hidden@example.org\r\nSubject: Hello\r\n\r\nBody\r\n";
    let uploaded = app
        .request(
            axum::http::Request::builder()
                .method("POST")
                .uri(format!("/api/jmap/upload/{account}"))
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "message/rfc822")
                .body(axum::body::Body::from(raw.to_vec()))
                .expect("upload request"),
        )
        .await
        .expect(StatusCode::CREATED);
    let blob_id = uploaded["blobId"].as_str().expect("blob id");

    let imported = app
        .json(
            "POST",
            "/api/jmap/",
            Some(&token),
            json!({
                "using": [CORE, MAIL],
                "methodCalls": [[
                    "Email/import",
                    {
                        "accountId": account,
                        "emails": {
                            "draft": {
                                "blobId": blob_id,
                                "mailboxIds": { drafts.to_string(): true },
                                "keywords": { "$draft": true }
                            }
                        }
                    },
                    "e"
                ]]
            }),
        )
        .await
        .expect(StatusCode::OK);
    let email_id = imported["methodResponses"][0][1]["created"]["draft"]["id"]
        .as_str()
        .expect("imported email id");

    let submitted = app
        .json(
            "POST",
            "/api/jmap/",
            Some(&token),
            json!({
                "using": [CORE, MAIL, SUBMISSION],
                "methodCalls": [[
                    "EmailSubmission/set",
                    {
                        "accountId": account,
                        "create": {
                            "send": {
                                "identityId": identity_id,
                                "emailId": email_id
                            }
                        },
                        "onSuccessDestroyEmail": [format!("#{email_id}")]
                    },
                    "s"
                ]]
            }),
        )
        .await
        .expect(StatusCode::OK);
    let created = &submitted["methodResponses"][0][1]["created"]["send"];
    assert!(
        created["id"].as_str().is_some_and(|id| id.starts_with('s')),
        "submission must be created: {submitted}"
    );
    assert!(submitted["methodResponses"][0][1]["notCreated"]["send"].is_null());

    let pending = app
        .state
        .repos
        .queue
        .list_by_status("pending", 20, 0)
        .await
        .expect("queue listing");
    assert!(
        pending
            .iter()
            .any(|entry| entry.recipient == "bob@example.org"),
        "the visible recipient must be queued: {pending:?}"
    );
    assert!(
        pending
            .iter()
            .any(|entry| entry.recipient == "hidden@example.org"),
        "the Bcc recipient must stay on the envelope: {pending:?}"
    );
    let sent = pending
        .iter()
        .find(|entry| entry.recipient == "bob@example.org")
        .expect("queued copy");
    let stored = app
        .state
        .repos
        .messages
        .find_by_id(ferroma_core::MessageId::new(sent.message_id))
        .await
        .expect("sent lookup")
        .expect("the Sent copy");
    let bytes = app
        .state
        .mail_service
        .maildir()
        .read(&stored.storage_path)
        .expect("stored bytes");
    let text = String::from_utf8_lossy(&bytes);
    assert!(
        !text.to_ascii_lowercase().contains("bcc:"),
        "the stored copy must not carry the Bcc header: {text}"
    );
    assert!(text.contains("Body"), "removing Bcc must keep the body: {text}");

    let source = email_id.parse::<i64>().expect("numeric email id");
    let draft = app
        .state
        .repos
        .messages
        .find_by_id(ferroma_core::MessageId::new(source))
        .await
        .expect("draft lookup");
    assert!(draft.is_none(), "the source draft must be destroyed");

    app.cleanup().await;
}

/// What an ordinary JMAP client does after it has a session: list the mailboxes,
/// save a draft, read its body back through a result reference, move it, and
/// learn about the move from `Email/changes` rather than by listing everything.
#[tokio::test]
async fn a_client_can_read_file_move_and_sync_mail() {
    require_database!();
    let app = TestApp::new().await;
    let admin = bootstrap_admin(&app).await;
    let (user_id, mailbox_id, management) =
        seed_account(&app, &admin, "example.net", "alice@example.net", PASSWORD).await;
    let token = jmap_token(&app, "alice@example.net").await;
    let account = format!("u{user_id}");
    let drafts = folder_id(&app, &management, mailbox_id, "Drafts").await;

    let before = app
        .json(
            "POST",
            "/api/jmap/",
            Some(&token),
            json!({
                "using": [CORE, MAIL],
                "methodCalls": [["Mailbox/get", {"accountId": account}, "b"]]
            }),
        )
        .await
        .expect(StatusCode::OK);
    let since = before["methodResponses"][0][1]["state"]
        .as_str()
        .expect("mailbox state")
        .to_string();
    let draft_role = before["methodResponses"][0][1]["list"]
        .as_array()
        .expect("mailboxes")
        .iter()
        .find(|mailbox| mailbox["id"] == drafts.to_string());
    assert_eq!(
        draft_role.and_then(|mailbox| mailbox["role"].as_str()),
        Some("drafts")
    );

    let written = app
        .json(
            "POST",
            "/api/jmap/",
            Some(&token),
            json!({
                "using": [CORE, MAIL],
                "methodCalls": [
                    ["Mailbox/set", {
                        "accountId": account,
                        "create": {"box": {"name": "Projects"}}
                    }, "m"],
                    ["Email/set", {
                        "accountId": account,
                        "create": {"draft": {
                            "mailboxIds": {"#box": true},
                            "keywords": {"$draft": true, "$seen": false},
                            "from": [{"email": "alice@example.net", "name": "Alice"}],
                            "to": [{"email": "bob@example.org"}],
                            "subject": "Re: Lunch",
                            "textBody": [{"partId": "text"}],
                            "bodyValues": {"text": {"value": "Tomorrow at noon."}}
                        }}
                    }, "c"],
                    ["Email/query", {
                        "accountId": account,
                        "filter": {"subject": "Lunch"}
                    }, "q"],
                    ["Email/get", {
                        "accountId": account,
                        "#ids": {"resultOf": "q", "name": "Email/query", "path": "/ids"},
                        "properties": ["subject", "textBody", "bodyValues", "keywords", "mailboxIds"],
                        "fetchTextBodyValues": true
                    }, "g"]
                ]
            }),
        )
        .await
        .expect(StatusCode::OK);
    let responses = &written["methodResponses"];
    assert_eq!(responses[0][0], "Mailbox/set", "{written}");
    let folder = responses[0][1]["created"]["box"]["id"]
        .as_str()
        .expect("created mailbox")
        .to_string();
    assert!(responses[0][1]["notCreated"]["box"].is_null(), "{written}");
    let email = responses[1][1]["created"]["draft"]["id"]
        .as_str()
        .expect("created draft")
        .to_string();
    assert_eq!(responses[3][0], "Email/get", "{written}");
    let got = &responses[3][1]["list"][0];
    assert_eq!(got["subject"], "Re: Lunch");
    assert_eq!(got["bodyValues"]["text"]["value"], "Tomorrow at noon.");
    assert_eq!(got["keywords"]["$draft"], true);
    assert_eq!(got["mailboxIds"][&folder], true);
    // A list view asks for the subject and must not be handed the body beside it.
    assert!(got.get("preview").is_none(), "properties must be honoured: {got}");

    let inbox = folder_id(&app, &management, mailbox_id, "INBOX").await;
    let moved = app
        .json(
            "POST",
            "/api/jmap/",
            Some(&token),
            json!({
                "using": [CORE, MAIL],
                "methodCalls": [[
                    "Email/set",
                    {"accountId": account, "update": {email.clone(): {"mailboxIds": {inbox.to_string(): true}}}},
                    "u"
                ]]
            }),
        )
        .await
        .expect(StatusCode::OK);
    assert!(
        moved["methodResponses"][0][1]["notUpdated"][&email].is_null(),
        "the move must be accepted: {moved}"
    );
    let after = moved["methodResponses"][0][1]["newState"]
        .as_str()
        .expect("new state");
    assert_ne!(since, after, "a move changes the state a client syncs from");

    let changes = app
        .json(
            "POST",
            "/api/jmap/",
            Some(&token),
            json!({
                "using": [CORE, MAIL],
                "methodCalls": [
                    ["Email/changes", {"accountId": account, "sinceState": since}, "e"],
                    ["Mailbox/changes", {"accountId": account, "sinceState": since}, "f"],
                    ["Email/query", {
                        "accountId": account,
                        "filter": {"inMailbox": inbox.to_string(), "text": "noon"},
                        "sort": [{"property": "subject", "isAscending": true}]
                    }, "q"]
                ]
            }),
        )
        .await
        .expect(StatusCode::OK);
    let email_changes = &changes["methodResponses"][0][1];
    assert!(
        email_changes["created"]
            .as_array()
            .is_some_and(|ids| ids.iter().any(|id| id == &email)),
        "the draft is new since the first state: {email_changes}"
    );
    assert_eq!(email_changes["oldState"], since);
    let mailbox_changes = &changes["methodResponses"][1][1];
    assert!(
        mailbox_changes["created"]
            .as_array()
            .is_some_and(|ids| ids.iter().any(|id| id == &folder)),
        "the new folder is created: {mailbox_changes}"
    );
    let ids = changes["methodResponses"][2][1]["ids"]
        .as_array()
        .expect("query ids");
    assert_eq!(ids, &vec![json!(email)], "the moved mail matches text and mailbox");

    // `$seen` means the message has been read. The old adapter treated it as unread.
    let unread = app
        .json(
            "POST",
            "/api/jmap/",
            Some(&token),
            json!({
                "using": [CORE, MAIL],
                "methodCalls": [[
                    "Email/query",
                    {"accountId": account, "filter": {"inMailbox": inbox.to_string(), "hasKeyword": "$seen"}},
                    "s"
                ]]
            }),
        )
        .await
        .expect(StatusCode::OK);
    assert_eq!(
        unread["methodResponses"][0][1]["ids"].as_array().map(Vec::len),
        Some(0),
        "an unseen draft must not match hasKeyword $seen: {unread}"
    );

    app.cleanup().await;
}
