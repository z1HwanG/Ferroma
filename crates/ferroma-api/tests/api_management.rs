//! End-to-end tests of the management API over a real PostgreSQL schema.
//!
//! Each test gets its own migrated schema (see `common/mod.rs`) and drives the real
//! router through `tower::ServiceExt::oneshot`, so what is asserted is the bytes a
//! client would actually receive.

mod common;

use axum::http::StatusCode;
use common::{folder_id, login, seed_account, TestApp};
use ferroma_core::Config;
use serde_json::json;

/// A password the auth policy accepts.
const PASSWORD: &str = "correct horse battery";

/// Seed the administrator account through the setup wizard.
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
    let body = response.expect(StatusCode::CREATED);
    body["access_token"]
        .as_str()
        .expect("setup returns a token")
        .to_string()
}

#[tokio::test]
async fn setup_wizard_creates_the_first_admin_and_then_refuses() {
    require_database!();
    let app = TestApp::new().await;

    let status = app.get("/api/v1/setup", None).await;
    assert_eq!(status.expect(StatusCode::OK)["required"], true);

    let token = bootstrap_admin(&app).await;
    assert!(!token.is_empty());

    // Once an admin exists the wizard is closed, for both verbs.
    let status = app.get("/api/v1/setup", None).await;
    assert_eq!(status.expect(StatusCode::OK)["required"], false);
    let again = app
        .json(
            "POST",
            "/api/v1/setup",
            None,
            json!({
                "email": "second@example.com",
                "password": PASSWORD,
                "domain": "example.com"
            }),
        )
        .await;
    assert_eq!(again.status, StatusCode::CONFLICT);
    assert_eq!(again.error_code(), "conflict");

    app.cleanup().await;
}

#[tokio::test]
async fn login_then_me_reports_the_account_and_its_addresses() {
    require_database!();
    let app = TestApp::new().await;
    let admin = bootstrap_admin(&app).await;

    let (user_id, mailbox_id, token) =
        seed_account(&app, &admin, "example.net", "alice@example.net", PASSWORD).await;

    let me = app.get("/api/v1/auth/me", Some(&token)).await;
    let body = me.expect(StatusCode::OK);
    assert_eq!(body["id"].as_i64(), Some(user_id));
    assert_eq!(body["email"], "alice@example.net");
    assert_eq!(body["is_admin"], false);
    let mailboxes = body["mailboxes"].as_array().expect("mailboxes is an array");
    assert_eq!(mailboxes.len(), 1);
    assert_eq!(mailboxes[0]["address"], "alice@example.net");
    assert_eq!(mailboxes[0]["is_primary"], true);
    assert_eq!(mailboxes[0]["id"].as_i64(), Some(mailbox_id));

    // Each address is a full mailbox record, not the reduced brief: the Webmail's
    // normaliser reads `quota_bytes` and `used_bytes` off it, and without them every
    // address it listed reported as empty.
    assert_eq!(mailboxes[0]["user_id"].as_i64(), Some(user_id));
    assert_eq!(mailboxes[0]["enabled"], true);
    assert_eq!(mailboxes[0]["used_bytes"], 0, "{body}");
    assert!(mailboxes[0]["created_at"].is_string(), "{body}");
    assert!(
        mailboxes[0].get("quota_bytes").is_some(),
        "quota_bytes is always present, null when the address inherits the account's: {body}"
    );

    // No password material may appear anywhere in the body.
    assert!(!me.text().contains("argon2"), "{}", me.text());
    assert!(!me.text().contains("password"), "{}", me.text());

    app.cleanup().await;
}

#[tokio::test]
async fn a_bad_password_is_unauthorized_and_an_unknown_account_is_indistinguishable() {
    require_database!();
    let app = TestApp::new().await;
    let admin = bootstrap_admin(&app).await;
    seed_account(&app, &admin, "example.net", "alice@example.net", PASSWORD).await;

    let wrong = app
        .json(
            "POST",
            "/api/v1/auth/login",
            None,
            json!({ "email": "alice@example.net", "password": "nope" }),
        )
        .await;
    assert_eq!(wrong.status, StatusCode::UNAUTHORIZED);
    assert_eq!(wrong.error_code(), "unauthorized");

    let unknown = app
        .json(
            "POST",
            "/api/v1/auth/login",
            None,
            json!({ "email": "nobody@example.net", "password": "nope" }),
        )
        .await;
    assert_eq!(unknown.status, StatusCode::UNAUTHORIZED);
    // The same code and the same message: the endpoint cannot enumerate accounts.
    assert_eq!(unknown.error_code(), "unauthorized");
    assert_eq!(
        wrong.json()["error"]["message"],
        unknown.json()["error"]["message"]
    );

    app.cleanup().await;
}

#[tokio::test]
async fn a_browser_login_sets_the_session_cookie_and_a_device_login_does_not() {
    require_database!();
    let app = TestApp::new().await;
    let admin = bootstrap_admin(&app).await;
    seed_account(&app, &admin, "example.net", "alice@example.net", PASSWORD).await;

    let browser = app
        .json(
            "POST",
            "/api/v1/auth/login",
            None,
            json!({ "email": "alice@example.net", "password": PASSWORD }),
        )
        .await;
    browser.expect(StatusCode::OK);
    let cookie = browser
        .header("set-cookie")
        .expect("a browser login sets the session cookie");
    assert!(cookie.contains("ferroma_session="), "{cookie}");
    assert!(cookie.contains("HttpOnly"), "{cookie}");
    assert!(cookie.contains("SameSite=Lax"), "{cookie}");

    let device = app
        .json(
            "POST",
            "/api/v1/auth/login",
            None,
            json!({
                "email": "alice@example.net",
                "password": PASSWORD,
                "device_name": "Firefox on Linux"
            }),
        )
        .await;
    device.expect(StatusCode::OK);
    assert!(
        device.header("set-cookie").is_none(),
        "a named device gets tokens, not a cookie"
    );

    app.cleanup().await;
}

#[tokio::test]
async fn the_session_cookie_authenticates_the_management_surface() {
    require_database!();
    let app = TestApp::new().await;
    let admin = bootstrap_admin(&app).await;
    seed_account(&app, &admin, "example.net", "alice@example.net", PASSWORD).await;

    let browser = app
        .json(
            "POST",
            "/api/v1/auth/login",
            None,
            json!({ "email": "alice@example.net", "password": PASSWORD }),
        )
        .await;
    let set_cookie = browser.header("set-cookie").expect("cookie");
    let pair = set_cookie
        .split(';')
        .next()
        .expect("name=value")
        .to_string();

    let request = axum::http::Request::builder()
        .method("GET")
        .uri("/api/v1/auth/me")
        .header(axum::http::header::COOKIE, pair)
        .body(axum::body::Body::empty())
        .expect("valid request");
    let me = app.request(request).await;
    let body = me.expect(StatusCode::OK);
    assert_eq!(body["email"], "alice@example.net");

    app.cleanup().await;
}

#[tokio::test]
async fn refresh_rotates_the_token_and_a_replay_revokes_the_family() {
    require_database!();
    let app = TestApp::new().await;
    let admin = bootstrap_admin(&app).await;
    seed_account(&app, &admin, "example.net", "alice@example.net", PASSWORD).await;

    let first = app
        .json(
            "POST",
            "/api/v1/auth/login",
            None,
            json!({ "email": "alice@example.net", "password": PASSWORD }),
        )
        .await
        .expect(StatusCode::OK);
    let refresh_token = first["refresh_token"]
        .as_str()
        .expect("a refresh token")
        .to_string();

    let rotated = app
        .json(
            "POST",
            "/api/v1/auth/refresh",
            None,
            json!({ "refresh_token": refresh_token }),
        )
        .await;
    let rotated_body = rotated.expect(StatusCode::OK);
    let new_token = rotated_body["refresh_token"]
        .as_str()
        .expect("a rotated refresh token")
        .to_string();
    assert_ne!(new_token, refresh_token);

    // Replaying the consumed token revokes every session and answers 401.
    let replay = app
        .json(
            "POST",
            "/api/v1/auth/refresh",
            None,
            json!({ "refresh_token": refresh_token }),
        )
        .await;
    assert_eq!(replay.status, StatusCode::UNAUTHORIZED);

    app.cleanup().await;
}

#[tokio::test]
async fn logout_revokes_the_session_and_the_token_stops_working() {
    require_database!();
    let app = TestApp::new().await;
    let admin = bootstrap_admin(&app).await;
    let (_user_id, _mailbox_id, token) =
        seed_account(&app, &admin, "example.net", "alice@example.net", PASSWORD).await;

    app.get("/api/v1/auth/me", Some(&token))
        .await
        .expect(StatusCode::OK);

    let logout = app.json("POST", "/api/v1/auth/logout", Some(&token), json!({})).await;
    assert_eq!(logout.status, StatusCode::NO_CONTENT);

    let after = app.get("/api/v1/auth/me", Some(&token)).await;
    assert_eq!(after.status, StatusCode::UNAUTHORIZED);

    app.cleanup().await;
}

#[tokio::test]
async fn changing_the_password_revokes_every_session() {
    require_database!();
    let app = TestApp::new().await;
    let admin = bootstrap_admin(&app).await;
    let (_user_id, _mailbox_id, token) =
        seed_account(&app, &admin, "example.net", "alice@example.net", PASSWORD).await;

    let changed = app
        .json(
            "POST",
            "/api/v1/auth/password",
            Some(&token),
            json!({ "current_password": PASSWORD, "new_password": "a different secret" }),
        )
        .await;
    assert_eq!(changed.status, StatusCode::NO_CONTENT);

    // The old session no longer works, and the old password no longer logs in.
    assert_eq!(
        app.get("/api/v1/auth/me", Some(&token)).await.status,
        StatusCode::UNAUTHORIZED
    );
    let old = app
        .json(
            "POST",
            "/api/v1/auth/login",
            None,
            json!({ "email": "alice@example.net", "password": PASSWORD }),
        )
        .await;
    assert_eq!(old.status, StatusCode::UNAUTHORIZED);

    let fresh = app
        .json(
            "POST",
            "/api/v1/auth/login",
            None,
            json!({ "email": "alice@example.net", "password": "a different secret" }),
        )
        .await;
    fresh.expect(StatusCode::OK);

    app.cleanup().await;
}

#[tokio::test]
async fn an_unauthenticated_request_is_unauthorized_not_forbidden() {
    require_database!();
    let app = TestApp::new().await;

    for path in [
        "/api/v1/auth/me",
        "/api/v1/mailboxes",
        "/api/v1/messages",
        "/api/v1/users",
        "/api/v1/storage",
    ] {
        let response = app.get(path, None).await;
        assert_eq!(
            response.status,
            StatusCode::UNAUTHORIZED,
            "{path} returned {}",
            response.text()
        );
        assert_eq!(response.error_code(), "unauthorized");
    }

    app.cleanup().await;
}

#[tokio::test]
async fn a_garbage_bearer_token_is_unauthorized_with_the_documented_envelope() {
    require_database!();
    let app = TestApp::new().await;

    let response = app.get("/api/v1/auth/me", Some("not.a.token")).await;
    assert_eq!(response.status, StatusCode::UNAUTHORIZED);
    let body = response.json();
    assert_eq!(body["error"]["code"], "unauthorized");
    assert!(body["error"]["message"].is_string());
    // A malformed token must never reveal why it failed in a way that helps forging.
    assert!(!response.text().contains("signature"), "{}", response.text());

    app.cleanup().await;
}

#[tokio::test]
async fn creating_a_domain_a_user_and_an_address_creates_the_standard_folders() {
    require_database!();
    let app = TestApp::new().await;
    let admin = bootstrap_admin(&app).await;

    let (_user_id, mailbox_id, token) =
        seed_account(&app, &admin, "example.org", "bob@example.org", PASSWORD).await;

    let folders = app
        .get(
            &format!("/api/v1/mailboxes/{mailbox_id}/folders"),
            Some(&token),
        )
        .await
        .expect(StatusCode::OK);
    let names: Vec<String> = folders["folders"]
        .as_array()
        .expect("folders")
        .iter()
        .filter_map(|folder| folder["name"].as_str().map(str::to_string))
        .collect();
    for expected in ["INBOX", "Sent", "Drafts", "Trash", "Junk", "Archive"] {
        assert!(names.contains(&expected.to_string()), "missing {expected}: {names:?}");
    }

    // `INBOX` carries no `special_use`; the others do.
    let inbox = folders["folders"]
        .as_array()
        .expect("folders")
        .iter()
        .find(|folder| folder["name"] == "INBOX")
        .expect("INBOX");
    assert!(inbox["special_use"].is_null(), "{inbox}");
    let sent = folders["folders"]
        .as_array()
        .expect("folders")
        .iter()
        .find(|folder| folder["name"] == "Sent")
        .expect("Sent");
    assert_eq!(sent["special_use"], "\\Sent");

    // The Maildir exists on disk.
    let maildir = app.data_dir().join("mail").join("example.org").join("bob");
    assert!(maildir.join("Maildir").join("cur").is_dir(), "{maildir:?}");
    assert!(maildir.join("Maildir").join(".Sent").join("cur").is_dir());

    app.cleanup().await;
}

#[tokio::test]
async fn an_address_needs_a_domain_that_exists() {
    require_database!();
    let app = TestApp::new().await;
    let admin = bootstrap_admin(&app).await;

    let user = app
        .json(
            "POST",
            "/api/v1/users",
            Some(&admin),
            json!({ "email": "carol@nowhere.test", "password": PASSWORD }),
        )
        .await
        .expect(StatusCode::CREATED);
    let user_id = user["id"].as_i64().expect("user id");

    let missing = app
        .json(
            "POST",
            &format!("/api/v1/users/{user_id}/mailboxes"),
            Some(&admin),
            json!({ "domain": "nowhere.test", "local_part": "carol" }),
        )
        .await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
    assert_eq!(missing.error_code(), "not_found");

    app.cleanup().await;
}

#[tokio::test]
async fn a_duplicate_address_is_a_conflict() {
    require_database!();
    let app = TestApp::new().await;
    let admin = bootstrap_admin(&app).await;

    let user = app
        .json(
            "POST",
            "/api/v1/users",
            Some(&admin),
            json!({ "email": "dave@example.com", "password": PASSWORD }),
        )
        .await
        .expect(StatusCode::CREATED);
    let user_id = user["id"].as_i64().expect("user id");

    // `POST /users` already created `dave@example.com` (the setup wizard created
    // `example.com`), so asking for it again is the documented conflict.
    let again = app
        .json(
            "POST",
            &format!("/api/v1/users/{user_id}/mailboxes"),
            Some(&admin),
            json!({ "domain": "example.com", "local_part": "dave" }),
        )
        .await;
    assert_eq!(again.status, StatusCode::CONFLICT);
    assert_eq!(again.error_code(), "conflict");

    app.cleanup().await;
}

#[tokio::test]
async fn creating_an_account_also_creates_its_primary_address() {
    require_database!();
    let app = TestApp::new().await;
    let admin = bootstrap_admin(&app).await;

    // The domain has to exist first; `example.com` is created by `bootstrap_admin`.
    let user = app
        .json(
            "POST",
            "/api/v1/users",
            Some(&admin),
            json!({ "email": "erin@example.com", "password": PASSWORD }),
        )
        .await
        .expect(StatusCode::CREATED);

    // The response names the address, so a client does not have to ask for it.
    let mailboxes = user["mailboxes"].as_array().expect("mailboxes key");
    assert_eq!(mailboxes.len(), 1, "{user}");
    assert_eq!(mailboxes[0]["address"], "erin@example.com");
    assert_eq!(mailboxes[0]["is_primary"], true);
    let mailbox_id = mailboxes[0]["id"].as_i64().expect("mailbox id");

    let token = login(&app, "erin@example.com", PASSWORD).await;

    // The account can actually use it: the address is listed and its folders exist,
    // which is what "the account works" means to the Webmail that opens right after.
    let listed = app
        .get("/api/v1/mailboxes", Some(&token))
        .await
        .expect(StatusCode::OK);
    assert_eq!(listed["mailboxes"][0]["address"], "erin@example.com");

    let folders = app
        .get(&format!("/api/v1/mailboxes/{mailbox_id}/folders"), Some(&token))
        .await
        .expect(StatusCode::OK);
    let names: Vec<&str> = folders["folders"]
        .as_array()
        .expect("folders")
        .iter()
        .filter_map(|folder| folder["name"].as_str())
        .collect();
    for expected in ["INBOX", "Sent", "Drafts", "Trash", "Junk", "Archive"] {
        assert!(names.contains(&expected), "missing {expected}: {names:?}");
    }

    app.cleanup().await;
}

#[tokio::test]
async fn creating_an_account_in_a_missing_domain_still_creates_the_account() {
    require_database!();
    let app = TestApp::new().await;
    let admin = bootstrap_admin(&app).await;

    // An administrator may legitimately exist before its domain does; the account is
    // created and the empty address list is how the console learns it still has work.
    let user = app
        .json(
            "POST",
            "/api/v1/users",
            Some(&admin),
            json!({ "email": "frank@nowhere.test", "password": PASSWORD }),
        )
        .await
        .expect(StatusCode::CREATED);
    assert_eq!(user["mailboxes"], json!([]));

    app.cleanup().await;
}

#[tokio::test]
async fn a_parent_child_folder_name_creates_the_parent_too() {
    require_database!();
    let app = TestApp::new().await;
    let admin = bootstrap_admin(&app).await;

    let (_user_id, mailbox_id, token) =
        seed_account(&app, &admin, "example.org", "gina@example.org", PASSWORD).await;

    let created = app
        .json(
            "POST",
            &format!("/api/v1/mailboxes/{mailbox_id}/folders"),
            Some(&token),
            json!({ "name": "Projects/2026/Q1" }),
        )
        .await
        .expect(StatusCode::CREATED);
    assert_eq!(created["name"], "Projects/2026/Q1");
    let leaf_parent = created["parent_id"].as_i64().expect("the leaf has a parent");

    let listed = app
        .get(&format!("/api/v1/mailboxes/{mailbox_id}/folders"), Some(&token))
        .await
        .expect(StatusCode::OK);
    let rows = listed["folders"].as_array().expect("folders");

    let projects = rows
        .iter()
        .find(|folder| folder["name"] == "Projects")
        .expect("Projects");
    assert!(projects["parent_id"].is_null(), "{projects}");
    let year = rows
        .iter()
        .find(|folder| folder["name"] == "Projects/2026")
        .expect("Projects/2026");
    assert_eq!(year["parent_id"].as_i64(), projects["id"].as_i64());
    let quarter = rows
        .iter()
        .find(|folder| folder["name"] == "Projects/2026/Q1")
        .expect("Projects/2026/Q1");
    assert_eq!(quarter["parent_id"].as_i64(), Some(leaf_parent));
    assert_eq!(quarter["parent_id"].as_i64(), year["id"].as_i64());

    // Creating a name that already exists is still the documented conflict, even though
    // the parents on the way to it are adopted.
    let again = app
        .json(
            "POST",
            &format!("/api/v1/mailboxes/{mailbox_id}/folders"),
            Some(&token),
            json!({ "name": "Projects/2026" }),
        )
        .await;
    assert_eq!(again.status, StatusCode::CONFLICT);

    app.cleanup().await;
}

#[tokio::test]
async fn admin_routes_reject_a_normal_user_with_forbidden() {
    require_database!();
    let app = TestApp::new().await;
    let admin = bootstrap_admin(&app).await;
    let (_user_id, _mailbox_id, token) =
        seed_account(&app, &admin, "example.net", "alice@example.net", PASSWORD).await;

    for (method, path) in [
        ("GET", "/api/v1/users"),
        ("GET", "/api/v1/domains"),
        ("GET", "/api/v1/queue"),
        ("GET", "/api/v1/storage"),
        ("GET", "/api/v1/audit"),
        ("GET", "/api/v1/settings"),
        ("GET", "/api/v1/logs"),
        ("GET", "/api/v1/devices"),
    ] {
        let response = app.get(path, Some(&token)).await;
        assert_eq!(
            response.status,
            StatusCode::FORBIDDEN,
            "{method} {path} returned {} — {}",
            response.status,
            response.text()
        );
        assert_eq!(response.error_code(), "forbidden", "{path}");
    }

    // ...and the admin gets through the same routes.
    let users = app.get("/api/v1/users", Some(&admin)).await;
    users.expect(StatusCode::OK);

    app.cleanup().await;
}

#[tokio::test]
async fn deleting_a_domain_refuses_while_addresses_exist_unless_forced() {
    require_database!();
    let app = TestApp::new().await;
    let admin = bootstrap_admin(&app).await;

    let domain = app
        .json(
            "POST",
            "/api/v1/domains",
            Some(&admin),
            json!({ "name": "keep.example" }),
        )
        .await
        .expect(StatusCode::CREATED);
    let domain_id = domain["id"].as_i64().expect("domain id");

    let user = app
        .json(
            "POST",
            "/api/v1/users",
            Some(&admin),
            json!({ "email": "eve@keep.example", "password": PASSWORD }),
        )
        .await
        .expect(StatusCode::CREATED);
    // `POST /users` creates `eve@keep.example` too, which is the address this test
    // needs the domain to be holding.
    assert_eq!(
        user["mailboxes"].as_array().map(Vec::len),
        Some(1),
        "{user}"
    );

    let refused = app
        .delete(&format!("/api/v1/domains/{domain_id}"), Some(&admin))
        .await;
    assert_eq!(refused.status, StatusCode::CONFLICT);
    assert_eq!(refused.error_code(), "conflict");

    let forced = app
        .delete(
            &format!("/api/v1/domains/{domain_id}?force=true"),
            Some(&admin),
        )
        .await;
    assert_eq!(forced.status, StatusCode::NO_CONTENT);

    app.cleanup().await;
}

#[tokio::test]
async fn the_health_endpoint_reports_ok_with_a_live_database() {
    require_database!();
    let app = TestApp::new().await;

    let response = app.get("/api/v1/health", None).await;
    let body = response.expect(StatusCode::OK);
    assert_eq!(body["status"], "ok");
    assert_eq!(body["database"]["ok"], true);
    assert!(body["database"]["server_version"].is_string(), "{body}");
    assert!(body["database"]["pool"]["max"].as_u64().unwrap_or(0) >= 1);
    assert_eq!(body["smtp"]["enabled"], true);
    assert_eq!(body["imap"]["enabled"], true);
    // The new dashboard blocks are present and numeric.
    assert!(body["clients"]["active_sessions"].is_number(), "{body}");
    assert!(body["clients"]["active_devices"].is_number(), "{body}");
    assert!(body["queue"]["pending"].is_number(), "{body}");
    assert!(body["queue"]["received_today"].is_number(), "{body}");
    assert!(body["queue"]["sent_today"].is_number(), "{body}");
    assert!(body["queue"]["cancelled"].is_number(), "{body}");
    assert!(body["queue"]["bounce_pending"].is_number(), "{body}");
    assert!(body["queue"]["bounce_processing"].is_number(), "{body}");

    app.cleanup().await;
}

/// A delivery report that cannot be delivered is visible to an operator.
///
/// The failed delivery and its outstanding bounce task are separate states: without a
/// count for the task, a sender that never receives a bounce looks exactly like an
/// ordinary failed delivery, and the one thing an operator must act on is invisible.
#[tokio::test]
async fn the_health_endpoint_counts_an_outstanding_bounce_task() {
    require_database!();
    let app = TestApp::new().await;
    let admin = bootstrap_admin(&app).await;
    let (_user_id, _mailbox_id, token) =
        seed_account(&app, &admin, "example.net", "alice@example.net", PASSWORD).await;

    let sent = app
        .json(
            "POST",
            "/api/v1/messages",
            Some(&token),
            json!({
                "from": "alice@example.net",
                "to": ["bob@example.org"],
                "subject": "bounce me",
                "text": "body"
            }),
        )
        .await
        .expect(StatusCode::OK);
    let message = ferroma_core::MessageId::new(sent["message_id"].as_i64().unwrap());

    // Fail the delivery and leave its report owed, exactly as the worker does:
    // claim the row, then record the outcome against that claim.
    let claimed = app.state.repos.queue.claim_due(10).await.unwrap();
    let entry = claimed
        .iter()
        .find(|entry| entry.message_id == message.get())
        .expect("the row was claimed");
    let settled = app.state
        .repos
        .queue
        .finish_claim(
            entry.queue_id(),
            entry.attempts,
            "failed",
            None,
            Some("550 no such user"),
            None,
            Some(550),
            Some("no such user"),
            true,
        )
        .await
        .unwrap();
    assert!(settled, "the claim must settle as failed");

    let body = app.get("/api/v1/health", None).await.expect(StatusCode::OK);
    assert_eq!(body["queue"]["failed"], 1, "{body}");
    assert_eq!(
        body["queue"]["bounce_pending"], 1,
        "the owed report must be visible: {body}"
    );
    assert_eq!(body["queue"]["bounce_processing"], 0, "{body}");

    app.cleanup().await;
}

#[tokio::test]
async fn the_health_endpoint_reports_degraded_when_the_database_is_gone() {
    require_database!();
    let app = TestApp::new().await;
    // A state that was never given a database handle: the health probe has nothing to
    // reach, which is exactly the "database is unreachable" case.
    let database = common::fresh_database().await;
    let repos = database.repos();
    let config = std::sync::Arc::new(ferroma_core::Config::default());
    let tokens = ferroma_auth::TokenService::new(
        "0123456789abcdef0123456789abcdef0123456789",
        3600,
        86_400,
        "localhost",
    )
    .expect("valid secret");
    let auth = std::sync::Arc::new(ferroma_auth::AuthService::with_defaults(
        repos.clone(),
        tokens.clone(),
        ferroma_core::Limits::default(),
    ));
    let sync = std::sync::Arc::new(ferroma_sync::SyncService::new(repos.clone(), 500, 30));
    let dead = ferroma_api::AppState::new(
        repos,
        config,
        tokens,
        auth,
        std::sync::Arc::new(ferroma_events::EventBus::with_defaults()),
        sync,
    );
    let dead_router = ferroma_api::build(dead);
    let response = tower::ServiceExt::oneshot(
        dead_router,
        axum::http::Request::builder()
            .method("GET")
            .uri("/api/v1/health")
            .body(axum::body::Body::empty())
            .expect("valid request"),
    )
    .await
    .expect("the router answers");
    let response = common::TestResponse::from_response(response).await;

    assert_eq!(response.status, StatusCode::SERVICE_UNAVAILABLE);
    let body = response.json();
    assert_eq!(body["status"], "degraded");
    assert_eq!(body["database"]["ok"], false);
    // The documented omission: no fabricated zeros.
    assert!(body.get("queue").is_none(), "{body}");
    assert!(body.get("clients").is_none(), "{body}");

    app.cleanup().await;
    database.cleanup().await;
}

#[tokio::test]
async fn well_known_autodiscovery_needs_no_authentication() {
    require_database!();
    let app = TestApp::new().await;

    let response = app.get("/.well-known/ferroma", None).await;
    let body = response.expect(StatusCode::OK);
    assert_eq!(body["api"], "http://localhost:8080/api/v1");
    assert_eq!(body["imap"]["host"], "localhost");
    assert_eq!(body["smtp"]["host"], "localhost");
    assert_eq!(body["protocol_version"], 1);
    assert!(body["web"].is_string());

    app.cleanup().await;
}

#[tokio::test]
async fn the_version_endpoint_is_unauthenticated_and_complete() {
    require_database!();
    let app = TestApp::new().await;

    let body = app
        .get("/api/v1/version", None)
        .await
        .expect(StatusCode::OK);
    assert!(body["version"].is_string());
    assert_eq!(body["protocol_version"], 1);
    assert!(body["git_sha"].is_string());
    assert!(body["built"].is_string());

    app.cleanup().await;
}

#[tokio::test]
async fn the_storage_report_counts_what_it_can_and_omits_the_rest() {
    require_database!();
    let app = TestApp::new().await;
    let admin = bootstrap_admin(&app).await;
    seed_account(&app, &admin, "example.net", "alice@example.net", PASSWORD).await;

    let body = app
        .get("/api/v1/storage", Some(&admin))
        .await
        .expect(StatusCode::OK);
    assert!(body["users"].as_i64().unwrap_or(0) >= 2, "{body}");
    assert!(body["domains"].as_i64().unwrap_or(0) >= 2, "{body}");
    assert!(body["mailboxes"].as_i64().unwrap_or(0) >= 1, "{body}");
    assert!(body["maildir_bytes"].is_number(), "{body}");
    assert!(body["attachment_bytes"].is_number(), "{body}");
    assert!(body["database_bytes"].is_number(), "{body}");

    let gc = app
        .json("POST", "/api/v1/storage/gc", Some(&admin), json!({}))
        .await;
    let gc_body = gc.expect(StatusCode::OK);
    assert!(gc_body["removed_attachments"].is_number(), "{gc_body}");
    assert!(gc_body["duration_ms"].is_number());

    app.cleanup().await;
}

#[tokio::test]
async fn settings_round_trip_through_the_database() {
    require_database!();
    let app = TestApp::new().await;
    let admin = bootstrap_admin(&app).await;

    let empty = app
        .get("/api/v1/settings", Some(&admin))
        .await
        .expect(StatusCode::OK);
    assert_eq!(empty["total"], 0);

    let put = app
        .json(
            "PUT",
            "/api/v1/settings/signup.enabled",
            Some(&admin),
            json!({ "value": false }),
        )
        .await;
    put.expect(StatusCode::OK);

    let listed = app
        .get("/api/v1/settings", Some(&admin))
        .await
        .expect(StatusCode::OK);
    assert_eq!(listed["total"], 1);
    assert_eq!(listed["items"][0]["key"], "signup.enabled");
    assert_eq!(listed["items"][0]["value"], false);

    // A key that could not round-trip through a URL path is refused.
    let bad = app
        .json(
            "PUT",
            "/api/v1/settings/bad%20key",
            Some(&admin),
            json!({ "value": 1 }),
        )
        .await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);

    app.cleanup().await;
}

#[tokio::test]
async fn the_audit_trail_records_administrative_actions_and_filters() {
    require_database!();
    let app = TestApp::new().await;
    let admin = bootstrap_admin(&app).await;

    app.json(
        "POST",
        "/api/v1/domains",
        Some(&admin),
        json!({ "name": "audit.example" }),
    )
    .await
    .expect(StatusCode::CREATED);

    let all = app
        .get("/api/v1/audit", Some(&admin))
        .await
        .expect(StatusCode::OK);
    assert!(all["total"].as_i64().unwrap_or(0) >= 1, "{all}");

    let filtered = app
        .get("/api/v1/audit?action=domain.created", Some(&admin))
        .await
        .expect(StatusCode::OK);
    assert_eq!(filtered["total"], 1);
    assert_eq!(filtered["items"][0]["action"], "domain.created");
    assert_eq!(filtered["items"][0]["target_type"], "domain");
    assert!(filtered["items"][0]["actor_user_id"].is_number());

    let none = app
        .get("/api/v1/audit?action=never.happened", Some(&admin))
        .await
        .expect(StatusCode::OK);
    assert_eq!(none["total"], 0);

    app.cleanup().await;
}

#[tokio::test]
async fn the_log_buffer_answers_with_its_own_metadata() {
    require_database!();
    let app = TestApp::new().await;
    let admin = bootstrap_admin(&app).await;

    let body = app
        .get("/api/v1/logs", Some(&admin))
        .await
        .expect(StatusCode::OK);
    assert_eq!(body["buffer_capacity"], 1000);
    assert!(body["buffer_entries"].is_number());
    assert_eq!(body["limit"], 100);
    assert_eq!(body["items"].as_array().map(Vec::len), Some(0));

    let bad_level = app
        .get("/api/v1/logs?level=verbose", Some(&admin))
        .await;
    assert_eq!(bad_level.status, StatusCode::BAD_REQUEST);

    app.cleanup().await;
}

#[tokio::test]
async fn the_domain_dns_report_has_the_documented_rows_and_scoring() {
    require_database!();
    let app = TestApp::new().await;
    let admin = bootstrap_admin(&app).await;

    let domain = app
        .json(
            "POST",
            "/api/v1/domains",
            Some(&admin),
            json!({ "name": "dns.example" }),
        )
        .await
        .expect(StatusCode::CREATED);
    let domain_id = domain["id"].as_i64().expect("domain id");

    let body = app
        .get(&format!("/api/v1/domains/{domain_id}/dns"), Some(&admin))
        .await
        .expect(StatusCode::OK);
    assert_eq!(body["domain"], "dns.example");
    let records = body["records"].as_array().expect("records");
    let kinds: Vec<&str> = records
        .iter()
        .filter_map(|record| record["kind"].as_str())
        .collect();
    for expected in ["MX", "A", "AAAA", "PTR", "SPF", "DKIM", "DMARC"] {
        assert!(kinds.contains(&expected), "missing {expected}: {kinds:?}");
    }
    for record in records {
        let status = record["status"].as_str().unwrap_or_default();
        assert!(
            ["ok", "warn", "fail", "skip"].contains(&status),
            "unexpected status {status}"
        );
    }
    assert!(body["score"].is_number());
    assert!(body["max_score"].is_number());

    app.cleanup().await;
}

#[tokio::test]
async fn an_unimplemented_dkim_pair_reports_not_found_rather_than_a_fake_record() {
    require_database!();
    let app = TestApp::new().await;
    let admin = bootstrap_admin(&app).await;

    let domain = app
        .json(
            "POST",
            "/api/v1/domains",
            Some(&admin),
            json!({ "name": "dkim.example" }),
        )
        .await
        .expect(StatusCode::CREATED);
    let domain_id = domain["id"].as_i64().expect("domain id");

    // No key pair exists yet, so the record cannot be published.
    let response = app
        .get(&format!("/api/v1/domains/{domain_id}/dkim"), Some(&admin))
        .await;
    assert_eq!(response.status, StatusCode::NOT_FOUND);

    app.cleanup().await;
}

#[tokio::test]
async fn the_standard_folder_set_is_created_for_every_new_address() {
    require_database!();
    let app = TestApp::new().await;
    let admin = bootstrap_admin(&app).await;
    let (_user_id, mailbox_id, token) =
        seed_account(&app, &admin, "example.net", "frank@example.net", PASSWORD).await;

    // Creating a custom folder works and shows up in the list.
    let created = app
        .json(
            "POST",
            &format!("/api/v1/mailboxes/{mailbox_id}/folders"),
            Some(&token),
            json!({ "name": "2026", "parent": "Archive" }),
        )
        .await;
    let body = created.expect(StatusCode::CREATED);
    assert_eq!(body["name"], "Archive/2026");

    let folder = folder_id(&app, &token, mailbox_id, "Archive/2026").await;
    assert!(folder > 0);

    app.cleanup().await;
}

#[tokio::test]
async fn inbox_cannot_be_renamed_or_deleted() {
    require_database!();
    let app = TestApp::new().await;
    let admin = bootstrap_admin(&app).await;
    let (_user_id, mailbox_id, token) =
        seed_account(&app, &admin, "example.net", "grace@example.net", PASSWORD).await;
    let inbox = folder_id(&app, &token, mailbox_id, "INBOX").await;

    let renamed = app
        .json(
            "PATCH",
            &format!("/api/v1/folders/{inbox}"),
            Some(&token),
            json!({ "name": "Home" }),
        )
        .await;
    assert_eq!(renamed.status, StatusCode::BAD_REQUEST);
    assert_eq!(renamed.error_code(), "invalid_input");

    let deleted = app
        .delete(&format!("/api/v1/folders/{inbox}"), Some(&token))
        .await;
    assert_eq!(deleted.status, StatusCode::BAD_REQUEST);

    app.cleanup().await;
}

#[tokio::test]
async fn a_non_admin_cannot_see_another_users_message_by_guessing_its_id() {
    require_database!();
    let app = TestApp::new().await;
    let admin = bootstrap_admin(&app).await;

    let (_alice, alice_mailbox, alice) =
        seed_account(&app, &admin, "example.net", "alice@example.net", PASSWORD).await;
    let (_bob, _bob_mailbox, bob) =
        seed_account(&app, &admin, "example.net", "bob@example.net", PASSWORD).await;

    // Alice sends herself a message so there is a row to guess.
    let sent = app
        .json(
            "POST",
            "/api/v1/messages",
            Some(&alice),
            json!({
                "from": "alice@example.net",
                "to": ["alice@example.net"],
                "subject": "private",
                "text": "nobody else may read this"
            }),
        )
        .await
        .expect(StatusCode::OK);
    let message_id = sent["message_id"].as_i64().expect("message id");

    // Alice can read it...
    app.get(&format!("/api/v1/messages/{message_id}"), Some(&alice))
        .await
        .expect(StatusCode::OK);

    // ...Bob gets 404, never 403: the API must not confirm the id exists.
    let stolen = app
        .get(&format!("/api/v1/messages/{message_id}"), Some(&bob))
        .await;
    assert_eq!(
        stolen.status,
        StatusCode::NOT_FOUND,
        "{}",
        stolen.text()
    );
    assert_eq!(stolen.error_code(), "not_found");

    // The same holds for every verb that takes a message id.
    assert_eq!(
        app.delete(&format!("/api/v1/messages/{message_id}"), Some(&bob))
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        app.json(
            "PATCH",
            &format!("/api/v1/messages/{message_id}"),
            Some(&bob),
            json!({ "seen": true })
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        app.get(&format!("/api/v1/messages/{message_id}/raw"), Some(&bob))
            .await
            .status,
        StatusCode::NOT_FOUND
    );

    // Alice's message is untouched: her sent copy carries `seen` and nothing else, so
    // Bob's attempts never reached it.
    let still_there = app
        .get(&format!("/api/v1/messages/{message_id}"), Some(&alice))
        .await
        .expect(StatusCode::OK);
    assert_eq!(
        still_there["flags"].as_str().unwrap_or_default(),
        "seen",
        "{still_there}"
    );

    // A mailbox id belonging to somebody else is a 404 too.
    assert_eq!(
        app.get(
            &format!("/api/v1/mailboxes/{alice_mailbox}/folders"),
            Some(&bob)
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );

    app.cleanup().await;
}

/* ------------------------------- the contract gaps the Admin console hits ------- */

#[tokio::test]
async fn an_unknown_api_path_answers_with_the_documented_envelope() {
    require_database!();
    let app = TestApp::new().await;

    // Without a fallback on the management router this fell through to the Webmail's
    // SPA fallback and answered `index.html` with `200 OK`: an API typo looked like a
    // page, and `admin/api.js` rendered "Request failed (HTTP 200)".
    let response = app.get("/api/v1/definitely-not-a-route", None).await;
    assert_eq!(response.status, StatusCode::NOT_FOUND);
    assert_eq!(response.error_code(), "not_found");
    assert!(
        response.json()["error"]["message"].is_string(),
        "{}",
        response.json()
    );

    app.cleanup().await;
}

#[tokio::test]
async fn a_known_path_with_the_wrong_method_answers_with_the_envelope() {
    require_database!();
    let app = TestApp::new().await;

    // `/health` is a `GET`; a bare axum `405` has an empty body, which a front-end can
    // only render as "Request failed (HTTP 405)".
    let response = app
        .json("POST", "/api/v1/health", None, json!({}))
        .await;
    assert_eq!(response.status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(response.error_code(), "method_not_allowed");

    app.cleanup().await;
}

#[tokio::test]
async fn listing_users_reports_each_accounts_addresses() {
    require_database!();
    let app = TestApp::new().await;
    let admin = bootstrap_admin(&app).await;
    let (user_id, _mailbox_id, _token) =
        seed_account(&app, &admin, "example.net", "alice@example.net", PASSWORD).await;

    // The console's user table prints an address count per row, so `GET /users` has to
    // carry the addresses. It used to send none, and the table said "addresses not
    // listed here" for ever.
    let page = app.get("/api/v1/users", Some(&admin)).await.expect(StatusCode::OK);
    let alice = page["items"]
        .as_array()
        .expect("items")
        .iter()
        .find(|user| user["email"] == "alice@example.net")
        .expect("alice is on the page");
    let addresses = alice["mailboxes"].as_array().expect("mailboxes");
    assert_eq!(addresses.len(), 1, "{alice}");
    assert_eq!(addresses[0]["address"], "alice@example.net", "{alice}");

    // An account with none says so with an empty list, which is a different answer
    // from an omitted key.
    let admin_row = page["items"]
        .as_array()
        .expect("items")
        .iter()
        .find(|user| user["email"] == "admin@example.com")
        .expect("the administrator is on the page");
    assert_eq!(admin_row["mailboxes"].as_array().map(Vec::len), Some(1), "{admin_row}");

    // The single-account read never asked for them, so the key is absent rather than
    // empty — that is what the client's `mailboxesKnown` flag reports.
    let single = app
        .get(&format!("/api/v1/users/{user_id}"), Some(&admin))
        .await
        .expect(StatusCode::OK);
    assert!(single.get("mailboxes").is_none(), "{single}");

    app.cleanup().await;
}

#[tokio::test]
async fn an_account_can_be_created_disabled() {
    require_database!();
    let app = TestApp::new().await;
    let admin = bootstrap_admin(&app).await;

    // The console's New-account dialog offers "Account enabled"; serde used to drop the
    // key, so unticking it still created a usable account.
    let created = app
        .json(
            "POST",
            "/api/v1/users",
            Some(&admin),
            json!({ "email": "carol@example.net", "password": PASSWORD, "enabled": false }),
        )
        .await
        .expect(StatusCode::CREATED);
    assert_eq!(created["enabled"], false, "{created}");
    let id = created["id"].as_i64().expect("id");

    let fetched = app
        .get(&format!("/api/v1/users/{id}"), Some(&admin))
        .await
        .expect(StatusCode::OK);
    assert_eq!(fetched["enabled"], false, "{fetched}");

    let enabled = app
        .json(
            "PATCH",
            &format!("/api/v1/users/{id}"),
            Some(&admin),
            json!({ "enabled": true }),
        )
        .await
        .expect(StatusCode::OK);
    assert_eq!(enabled["enabled"], true, "{enabled}");

    // Omitting the field still creates a usable account.
    let defaulted = app
        .json(
            "POST",
            "/api/v1/users",
            Some(&admin),
            json!({ "email": "dave@example.net", "password": PASSWORD }),
        )
        .await
        .expect(StatusCode::CREATED);
    assert_eq!(defaulted["enabled"], true, "{defaulted}");

    app.cleanup().await;
}

#[tokio::test]
async fn the_audit_trail_names_the_acting_account() {
    require_database!();
    let app = TestApp::new().await;
    let admin = bootstrap_admin(&app).await;

    app.json(
        "POST",
        "/api/v1/users",
        Some(&admin),
        json!({ "email": "carol@example.net", "password": PASSWORD }),
    )
    .await
    .expect(StatusCode::CREATED);

    // The console's audit table has an Actor column and reads an address, so the row
    // carries one. It used to send only `actor_user_id`, and the column showed `7`.
    let page = app
        .get("/api/v1/audit?action=user.created", Some(&admin))
        .await
        .expect(StatusCode::OK);
    let row = page["items"]
        .as_array()
        .expect("items")
        .first()
        .expect("an audit row")
        .clone();
    assert_eq!(row["actor"], "admin@example.com", "{row}");
    assert!(row["actor_user_id"].is_i64(), "{row}");

    app.cleanup().await;
}

#[tokio::test]
async fn the_setup_wizard_is_not_found_when_it_is_disabled() {
    require_database!();
    let mut config = Config::default();
    config.api.enable_setup_wizard = false;
    let app = TestApp::with_config(config).await;

    // "Disabled entirely" means the endpoint is not there. Answering
    // `200 {required:false}` made the console tell an operator who has no administrator
    // that one already existed, and left its "disabled" panel dead code.
    let status = app.get("/api/v1/setup", None).await;
    assert_eq!(status.status, StatusCode::NOT_FOUND);
    assert_eq!(status.error_code(), "not_found");

    let posted = app
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
    assert_eq!(posted.status, StatusCode::NOT_FOUND, "{}", posted.json());
    assert_eq!(posted.error_code(), "not_found");

    app.cleanup().await;
}

#[tokio::test]
async fn the_setup_wizard_stores_a_hostname_the_server_does_not_advertise_yet() {
    require_database!();
    let app = TestApp::new().await;

    // The wizard shows the hostname the running configuration advertises, because the
    // process cannot rewrite it; the operator confirms or replaces it.
    let status = app.get("/api/v1/setup", None).await.expect(StatusCode::OK);
    assert_eq!(status["required"], true, "{status}");
    assert_eq!(status["hostname"], "localhost", "{status}");
    assert!(status["public_url"].is_string(), "{status}");

    // Replacing it is not refused: the value is written to the `settings` table and the
    // server adopts it on its next start. A `400` here is what made the wizard useless on
    // an install that had not been told its hostname through the environment yet.
    let accepted = app
        .json(
            "POST",
            "/api/v1/setup",
            None,
            json!({
                "email": "admin@example.com",
                "password": PASSWORD,
                "hostname": "mail.example.com",
                "public_url": "https://mail.example.com/",
                "domain": "example.com"
            }),
        )
        .await;
    let body = accepted.expect(StatusCode::CREATED);
    assert_eq!(body["applied"]["hostname"], "mail.example.com", "{body}");
    assert_eq!(
        body["applied"]["public_url"], "https://mail.example.com",
        "the trailing slash is normalised away: {body}"
    );
    assert_eq!(body["applied"]["restart_required"], true, "{body}");
    // The token pair stays at the top level, where `setTokens` reads it.
    assert!(body["access_token"].is_string(), "{body}");

    let stored = app
        .db()
        .scalar::<serde_json::Value>("SELECT value FROM settings WHERE key = 'server.hostname'")
        .await
        .expect("the hostname was stored");
    assert_eq!(stored, json!("mail.example.com"));

    app.cleanup().await;
}

#[tokio::test]
async fn the_setup_wizard_omits_settings_that_agree_with_the_configuration() {
    require_database!();
    let app = TestApp::new().await;

    // Agreeing with the running configuration is the ordinary path, and it must not
    // write a settings row for every field the form happened to prefill.
    let accepted = app
        .json(
            "POST",
            "/api/v1/setup",
            None,
            json!({
                "email": "admin@example.com",
                "password": PASSWORD,
                "hostname": "localhost",
                "public_url": "http://localhost:8080",
                "domain": "example.com"
            }),
        )
        .await;
    let body = accepted.expect(StatusCode::CREATED);
    assert_eq!(body["applied"]["restart_required"], false, "{body}");
    assert!(body["applied"].get("hostname").is_none(), "{body}");
    assert_eq!(app.db().count("settings").await, 0, "nothing to store");

    app.cleanup().await;
}

/* ------------------------------------------------------- Accept-Language negotiation */

#[tokio::test]
async fn accept_language_selects_the_language_of_the_error_message() {
    require_database!();
    use axum::body::Body;
    use axum::http::{header, Request as HttpRequest};

    let app = TestApp::new().await;
    let admin = bootstrap_admin(&app).await;

    let with_language = |language: &'static str| {
        let admin = admin.clone();
        let app = &app;
        async move {
            let request = HttpRequest::builder()
                .method("GET")
                .uri("/api/v1/domains/9999")
                .header(header::AUTHORIZATION, format!("Bearer {admin}"))
                .header(header::ACCEPT_LANGUAGE, language)
                .body(Body::empty())
                .expect("valid request");
            app.request(request).await
        }
    };

    let english = with_language("en-US,en;q=0.9").await;
    assert_eq!(english.status, StatusCode::NOT_FOUND);
    assert_eq!(english.error_code(), "not_found");
    assert_eq!(
        english.json()["error"]["message"],
        "not found: domain 9999",
        "{}",
        english.json()
    );

    let chinese = with_language("zh-CN,zh;q=0.9,en;q=0.8").await;
    assert_eq!(chinese.status, StatusCode::NOT_FOUND);
    // `code` is machine-readable and language-neutral; only `message` follows the
    // header, so a client switching languages never has to change its branching.
    assert_eq!(chinese.error_code(), "not_found");
    assert_eq!(
        chinese.json()["error"]["message"],
        "未找到：域名 9999",
        "{}",
        chinese.json()
    );

    // A request that asks for nothing gets English, which is the documented default.
    let default = app.get("/api/v1/domains/9999", Some(&admin)).await;
    assert_eq!(
        default.json()["error"]["message"],
        "not found: domain 9999",
        "{}",
        default.json()
    );

    // A message a person reads rather than a machine, from the auth service.
    let refused = app
        .json_with_headers(
            "POST",
            "/api/v1/auth/login",
            None,
            json!({ "email": "admin@example.com", "password": "not the password" }),
            &[("accept-language", "zh-CN")],
        )
        .await;
    assert_eq!(refused.status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        refused.json()["error"]["message"],
        "未授权：邮箱地址或密码不正确",
        "{}",
        refused.json()
    );

    app.cleanup().await;
}

// ---------------------------------------------------------------------------
// Second factors and application passwords
// ---------------------------------------------------------------------------

/// The current TOTP code for a secret, as an authenticator app would show it.
fn totp_code(secret: &str) -> String {
    ferroma_auth::totp::current_code(secret, std::time::SystemTime::now()).expect("a code")
}

#[tokio::test]
async fn a_second_factor_can_be_enrolled_confirmed_and_removed() {
    require_database!();
    let app = TestApp::new().await;
    let admin = bootstrap_admin(&app).await;
    let (_user_id, _mailbox_id, token) =
        seed_account(&app, &admin, "example.net", "alice@example.net", PASSWORD).await;

    // Nothing enrolled yet.
    let status = app.get("/api/v1/auth/totp", Some(&token)).await;
    let body = status.expect(StatusCode::OK);
    assert_eq!(body["status"], "disabled");
    assert_eq!(body["recovery_codes_left"], 0);

    // Enroll: a secret and a scannable URI come back once.
    let enrolled = app
        .json("POST", "/api/v1/auth/totp/enroll", Some(&token), json!({}))
        .await
        .expect(StatusCode::OK);
    let secret = enrolled["secret"].as_str().expect("a secret").to_string();
    assert!(enrolled["uri"].as_str().unwrap_or_default().starts_with("otpauth://totp/"));

    // Pending, not enforced: the password still logs in.
    assert_eq!(
        app.get("/api/v1/auth/totp", Some(&token)).await.expect(StatusCode::OK)["status"],
        "pending"
    );
    let _ = login(&app, "alice@example.net", PASSWORD).await;

    // A wrong code does not confirm it.
    let refused = app
        .json("POST", "/api/v1/auth/totp/confirm", Some(&token), json!({ "code": "000000" }))
        .await;
    assert_eq!(refused.status, StatusCode::UNAUTHORIZED);

    let confirmed = app
        .json("POST", "/api/v1/auth/totp/confirm", Some(&token), json!({ "code": totp_code(&secret) }))
        .await
        .expect(StatusCode::OK);
    let codes: Vec<String> = confirmed["recovery_codes"]
        .as_array()
        .expect("recovery codes")
        .iter()
        .filter_map(|value| value.as_str().map(str::to_string))
        .collect();
    assert_eq!(codes.len(), ferroma_auth::totp::RECOVERY_CODE_COUNT);

    let enabled = app.get("/api/v1/auth/totp", Some(&token)).await.expect(StatusCode::OK);
    assert_eq!(enabled["status"], "enabled");
    assert_eq!(
        enabled["recovery_codes_left"],
        ferroma_auth::totp::RECOVERY_CODE_COUNT as i64
    );

    // The password alone no longer logs in, and the answer says which half is missing.
    let gated = app
        .json("POST", "/api/v1/auth/login", None, json!({
            "email": "alice@example.net", "password": PASSWORD
        }))
        .await;
    assert_eq!(gated.status, StatusCode::UNAUTHORIZED);
    assert_eq!(gated.error_code(), "totp_required");

    // With the code it does.
    let logged_in = app
        .json("POST", "/api/v1/auth/login", None, json!({
            "email": "alice@example.net", "password": PASSWORD, "totp": totp_code(&secret)
        }))
        .await;
    assert_eq!(logged_in.status, StatusCode::OK);

    // Turning it off needs the account password, not just the session.
    let without_password = app
        .json("POST", "/api/v1/auth/totp/disable", Some(&token), json!({ "password": "wrong" }))
        .await;
    assert_eq!(without_password.status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        app.get("/api/v1/auth/totp", Some(&token)).await.expect(StatusCode::OK)["status"],
        "enabled",
        "a failed disable must leave the factor in place"
    );

    let disabled = app
        .json("POST", "/api/v1/auth/totp/disable", Some(&token), json!({ "password": PASSWORD }))
        .await
        .expect(StatusCode::OK);
    assert_eq!(disabled["status"], "disabled");

    // And the password works alone again.
    let _ = login(&app, "alice@example.net", PASSWORD).await;
    app.cleanup().await;
}

#[tokio::test]
async fn an_application_password_is_minted_listed_and_revoked() {
    require_database!();
    let app = TestApp::new().await;
    let admin = bootstrap_admin(&app).await;
    let (_user_id, _mailbox_id, token) =
        seed_account(&app, &admin, "example.net", "alice@example.net", PASSWORD).await;

    let empty = app
        .get("/api/v1/auth/app-passwords", Some(&token))
        .await
        .expect(StatusCode::OK);
    assert_eq!(empty["items"].as_array().map(Vec::len), Some(0));

    let created = app
        .json("POST", "/api/v1/auth/app-passwords", Some(&token), json!({ "label": "Thunderbird" }))
        .await
        .expect(StatusCode::CREATED);
    let secret = created["secret"].as_str().expect("the secret").to_string();
    let id = created["id"].as_i64().expect("an id");
    assert!(secret.starts_with("ap_"));
    assert_eq!(created["label"], "Thunderbird");
    assert!(created["last_used_at"].is_null());

    // It is a working credential for a client that cannot present a code.
    let used = app
        .json("POST", "/api/v1/auth/login", None, json!({
            "email": "alice@example.net", "password": secret
        }))
        .await;
    assert_eq!(used.status, StatusCode::OK);

    let listed = app
        .get("/api/v1/auth/app-passwords", Some(&token))
        .await
        .expect(StatusCode::OK);
    assert_eq!(listed["items"].as_array().map(Vec::len), Some(1));
    assert_eq!(listed["items"][0]["label"], "Thunderbird");
    assert!(
        listed["items"][0]["last_used_at"].is_string(),
        "a used password must be stamped: {listed}"
    );

    let revoked = app
        .delete(&format!("/api/v1/auth/app-passwords/{id}"), Some(&token))
        .await;
    assert_eq!(revoked.status, StatusCode::NO_CONTENT);

    // Revoking twice is a 404, not a silent success.
    let again = app
        .delete(&format!("/api/v1/auth/app-passwords/{id}"), Some(&token))
        .await;
    assert_eq!(again.status, StatusCode::NOT_FOUND);

    // And the secret stops working.
    let refused = app
        .json("POST", "/api/v1/auth/login", None, json!({
            "email": "alice@example.net", "password": secret
        }))
        .await;
    assert_eq!(refused.status, StatusCode::UNAUTHORIZED);

    // A blank label is refused rather than stored.
    let blank = app
        .json("POST", "/api/v1/auth/app-passwords", Some(&token), json!({ "label": "   " }))
        .await;
    assert_eq!(blank.status, StatusCode::BAD_REQUEST);

    app.cleanup().await;
}

#[tokio::test]
async fn an_administrator_can_see_and_revoke_but_not_clear_a_second_factor() {
    require_database!();
    let app = TestApp::new().await;
    let admin = bootstrap_admin(&app).await;
    let (user_id, _mailbox_id, token) =
        seed_account(&app, &admin, "example.net", "alice@example.net", PASSWORD).await;

    // Disabled to begin with.
    let status = app
        .get(&format!("/api/v1/users/{user_id}/security"), Some(&admin))
        .await
        .expect(StatusCode::OK);
    assert_eq!(status["totp_status"], "disabled");
    assert_eq!(status["recovery_codes_left"], 0);
    assert_eq!(status["app_passwords"].as_array().map(Vec::len), Some(0));

    // Enroll and confirm through the account's own session, so the administrator is
    // looking at state the user created rather than state the test wrote.
    let enrolled = app
        .json("POST", "/api/v1/auth/totp/enroll", Some(&token), json!({}))
        .await
        .expect(StatusCode::OK);
    let secret = enrolled["secret"].as_str().expect("a secret").to_string();
    app.json(
        "POST",
        "/api/v1/auth/totp/confirm",
        Some(&token),
        json!({ "code": totp_code(&secret) }),
    )
    .await
    .expect(StatusCode::OK);
    let created = app
        .json("POST", "/api/v1/auth/app-passwords", Some(&token), json!({ "label": "Phone" }))
        .await
        .expect(StatusCode::CREATED);
    let app_id = created["id"].as_i64().expect("an id");

    let after = app
        .get(&format!("/api/v1/users/{user_id}/security"), Some(&admin))
        .await
        .expect(StatusCode::OK);
    assert_eq!(after["totp_status"], "enabled");
    assert_eq!(
        after["recovery_codes_left"],
        ferroma_auth::totp::RECOVERY_CODE_COUNT as i64
    );
    assert_eq!(after["app_passwords"].as_array().map(Vec::len), Some(1));
    assert_eq!(after["app_passwords"][0]["label"], "Phone");

    // There is no route that clears the factor, and asking for one is a `404`/`405`,
    // not a silent success: this is the property the whole design rests on.
    for method in ["POST", "DELETE", "PATCH"] {
        let attempt = app
            .json(
                method,
                &format!("/api/v1/users/{user_id}/totp"),
                Some(&admin),
                json!({}),
            )
            .await;
        assert!(
            attempt.status == StatusCode::NOT_FOUND || attempt.status == StatusCode::METHOD_NOT_ALLOWED,
            "{method} /users/{{id}}/totp answered {} — an admin session must not be able to clear a second factor",
            attempt.status
        );
    }
    // The factor is still enforced afterwards.
    let still_gated = app
        .json("POST", "/api/v1/auth/login", None, json!({
            "email": "alice@example.net", "password": PASSWORD
        }))
        .await;
    assert_eq!(still_gated.error_code(), "totp_required");

    // Revoking the application password is the one thing an administrator may do.
    let revoked = app
        .delete(&format!("/api/v1/users/{user_id}/app-passwords/{app_id}"), Some(&admin))
        .await;
    assert_eq!(revoked.status, StatusCode::NO_CONTENT);
    let listed = app
        .get(&format!("/api/v1/users/{user_id}/security"), Some(&admin))
        .await
        .expect(StatusCode::OK);
    assert_eq!(listed["totp_status"], "enabled", "revoking must not touch the factor");
    assert!(
        listed["app_passwords"][0]["revoked_at"].is_string(),
        "{listed}"
    );

    // Revoking again, an unknown id, and an unknown user are all `404`.
    let again = app
        .delete(&format!("/api/v1/users/{user_id}/app-passwords/{app_id}"), Some(&admin))
        .await;
    assert_eq!(again.status, StatusCode::NOT_FOUND);
    let unknown_id = app
        .delete(&format!("/api/v1/users/{user_id}/app-passwords/999999"), Some(&admin))
        .await;
    assert_eq!(unknown_id.status, StatusCode::NOT_FOUND);
    let unknown_user = app
        .get("/api/v1/users/999999/security", Some(&admin))
        .await;
    assert_eq!(unknown_user.status, StatusCode::NOT_FOUND);

    // And a plain user cannot read another account's security state.
    let (_bob_id, _bob_mailbox, bob) =
        seed_account(&app, &admin, "example.net", "bob@example.net", PASSWORD).await;
    let refused = app
        .get(&format!("/api/v1/users/{user_id}/security"), Some(&bob))
        .await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN);

    app.cleanup().await;
}
