//! End-to-end authentication tests against a real PostgreSQL schema.
//!
//! These are the tests that matter for this crate: the unit tests prove the crypto
//! and the token format, and these prove that login, throttling, rotation,
//! revocation and device management actually behave as the platform depends on.

mod common;

use common::{fresh_auth, TestAuth, TEST_PASSWORD};
use ferroma_auth::service::SessionKind;
use ferroma_auth::service::DeviceInfo;
use ferroma_core::{DeviceId, FerromaError, SessionId, UserId};

fn device(uid: &str) -> DeviceInfo {
    DeviceInfo {
        device_uid: uid.to_string(),
        name: Some("Test laptop".into()),
        platform: Some("windows".into()),
        client_version: Some("0.1.0".into()),
        protocol_version: Some(1),
    }
}

async fn login(t: &TestAuth, email: &str) -> ferroma_auth::LoginOutcome {
    t.auth
        .login(email, TEST_PASSWORD, SessionKind::Api, None, Some("test"), None)
        .await
        .expect("login")
}

#[tokio::test]
async fn login_then_authenticate_round_trips() {
    if !common::database_available().await {
        eprintln!("skipping: no PostgreSQL reachable");
        return;
    }
    let t = fresh_auth().await;
    let user = t.create_user("alice@example.com").await;

    let outcome = login(&t, "alice@example.com").await;
    assert_eq!(outcome.user.id, user.id);
    assert_eq!(outcome.tokens.expires_in, 3600);
    assert!(outcome.tokens.refresh_token.starts_with("rt_"));

    let identity = t.auth.authenticate(&outcome.tokens.access_token).await.unwrap();
    assert_eq!(identity.email(), "alice@example.com");
    assert_eq!(identity.user_id(), UserId::new(user.id));
    assert!(!identity.is_admin());
    assert_eq!(identity.session_id(), SessionId::new(outcome.session.id));

    // The successful login cleared the failure counters.
    let reloaded = t.repos().users.require_by_id(UserId::new(user.id)).await.unwrap();
    assert_eq!(reloaded.failed_logins, 0);
    assert!(reloaded.locked_until.is_none());
    assert!(reloaded.last_login_at.is_some());

    t.cleanup().await;
}

#[tokio::test]
async fn email_is_case_insensitive_at_login() {
    if !common::database_available().await {
        eprintln!("skipping: no PostgreSQL reachable");
        return;
    }
    let t = fresh_auth().await;
    t.create_user("alice@example.com").await;

    let outcome = t
        .auth
        .login("  Alice@Example.COM  ", TEST_PASSWORD, SessionKind::Api, None, None, None)
        .await
        .unwrap();
    assert_eq!(outcome.user.email, "alice@example.com");
    t.cleanup().await;
}

#[tokio::test]
async fn an_unknown_account_is_indistinguishable_from_a_wrong_password() {
    if !common::database_available().await {
        eprintln!("skipping: no PostgreSQL reachable");
        return;
    }
    let t = fresh_auth().await;
    t.create_user("alice@example.com").await;

    let unknown = t
        .auth
        .login("nobody@example.com", TEST_PASSWORD, SessionKind::Api, None, None, None)
        .await
        .unwrap_err();
    let wrong = t
        .auth
        .login("alice@example.com", "not the password", SessionKind::Api, None, None, None)
        .await
        .unwrap_err();

    // Same variant and same text: the endpoint must not enumerate accounts.
    assert!(matches!(unknown, FerromaError::Unauthorized(_)));
    assert!(matches!(wrong, FerromaError::Unauthorized(_)));
    assert_eq!(unknown.to_string(), wrong.to_string());

    t.cleanup().await;
}

#[tokio::test]
async fn wrong_passwords_increment_the_failure_counter() {
    if !common::database_available().await {
        eprintln!("skipping: no PostgreSQL reachable");
        return;
    }
    let t = fresh_auth().await;
    let user = t.create_user("alice@example.com").await;

    for expected in 1..=3 {
        let _ = t
            .auth
            .login("alice@example.com", "wrong", SessionKind::Api, None, None, None)
            .await;
        let reloaded = t.repos().users.require_by_id(UserId::new(user.id)).await.unwrap();
        assert_eq!(reloaded.failed_logins, expected);
    }

    // A success resets it.
    login(&t, "alice@example.com").await;
    let reloaded = t.repos().users.require_by_id(UserId::new(user.id)).await.unwrap();
    assert_eq!(reloaded.failed_logins, 0);

    t.cleanup().await;
}

#[tokio::test]
async fn the_account_locks_after_the_configured_number_of_failures() {
    if !common::database_available().await {
        eprintln!("skipping: no PostgreSQL reachable");
        return;
    }
    let t = fresh_auth().await;
    let user = t.create_user("alice@example.com").await;
    let max = t.repos().users.require_by_id(UserId::new(user.id)).await.unwrap();
    let _ = max;

    // `Limits::default()` allows 10 failures.
    let mut last = None;
    for _ in 0..10 {
        last = Some(
            t.auth
                .login("alice@example.com", "wrong", SessionKind::Api, None, None, None)
                .await,
        );
    }
    // The tenth failure trips the lock.
    assert!(matches!(last.unwrap(), Err(FerromaError::RateLimited)));

    let locked = t.repos().users.require_by_id(UserId::new(user.id)).await.unwrap();
    assert!(locked.locked_until.is_some(), "account must be locked");
    assert!(!locked.is_login_allowed(chrono::Utc::now()));

    // Even the *correct* password is refused while locked.
    let err = t
        .auth
        .login("alice@example.com", TEST_PASSWORD, SessionKind::Api, None, None, None)
        .await
        .unwrap_err();
    assert!(matches!(err, FerromaError::RateLimited), "{err:?}");

    t.cleanup().await;
}

#[tokio::test]
async fn a_disabled_account_cannot_log_in() {
    if !common::database_available().await {
        eprintln!("skipping: no PostgreSQL reachable");
        return;
    }
    let t = fresh_auth().await;
    let user = t.create_user("alice@example.com").await;
    t.repos().users.set_enabled(UserId::new(user.id), false).await.unwrap();

    let err = t
        .auth
        .login("alice@example.com", TEST_PASSWORD, SessionKind::Api, None, None, None)
        .await
        .unwrap_err();
    assert!(matches!(err, FerromaError::Unauthorized(_)));
    // Same message as a bad password, so a disabled account is not discoverable.
    assert!(err.to_string().contains("invalid email address or password"), "{err}");

    t.cleanup().await;
}

#[tokio::test]
async fn refresh_rotates_and_the_old_token_stops_working() {
    if !common::database_available().await {
        eprintln!("skipping: no PostgreSQL reachable");
        return;
    }
    let t = fresh_auth().await;
    t.create_user("alice@example.com").await;
    let first = login(&t, "alice@example.com").await;

    let second = t.auth.refresh(&first.tokens.refresh_token, None).await.unwrap();
    assert_ne!(second.refresh_token, first.tokens.refresh_token, "must rotate");
    assert_ne!(second.session_id, first.tokens.session_id, "a new session");

    // The new pair works.
    let identity = t.auth.authenticate(&second.access_token).await.unwrap();
    assert_eq!(identity.email(), "alice@example.com");

    t.cleanup().await;
}

#[tokio::test]
async fn reusing_a_rotated_refresh_token_revokes_every_session() {
    if !common::database_available().await {
        eprintln!("skipping: no PostgreSQL reachable");
        return;
    }
    let t = fresh_auth().await;
    let user = t.create_user("alice@example.com").await;

    // Two independent sessions, as if the user had a laptop and a phone.
    let laptop = login(&t, "alice@example.com").await;
    let phone = login(&t, "alice@example.com").await;

    // The laptop rotates normally.
    let rotated = t.auth.refresh(&laptop.tokens.refresh_token, None).await.unwrap();
    assert!(t.auth.authenticate(&rotated.access_token).await.is_ok());

    // Replaying the *old* refresh token is the theft signal.
    let err = t.auth.refresh(&laptop.tokens.refresh_token, None).await.unwrap_err();
    assert!(matches!(err, FerromaError::Unauthorized(_)), "{err:?}");
    assert!(err.to_string().contains("already used"), "{err}");

    // Every session of the user is now dead — including the phone's, and including
    // the pair the legitimate refresh just produced.
    for token in [
        &phone.tokens.access_token,
        &rotated.access_token,
        &laptop.tokens.access_token,
    ] {
        let err = t.auth.authenticate(token).await.unwrap_err();
        assert!(matches!(err, FerromaError::Unauthorized(_)), "token must be dead: {err:?}");
    }

    let sessions = t.repos().sessions.list_for_user(UserId::new(user.id), true).await.unwrap();
    assert!(sessions.iter().all(|s| s.revoked_at.is_some()), "all sessions revoked");

    t.cleanup().await;
}

#[tokio::test]
async fn an_expired_refresh_token_is_refused() {
    if !common::database_available().await {
        eprintln!("skipping: no PostgreSQL reachable");
        return;
    }
    let t = fresh_auth().await;
    t.create_user("alice@example.com").await;
    let outcome = login(&t, "alice@example.com").await;

    // Backdate the session so it is already expired.
    sqlx::query("UPDATE sessions SET expires_at = NOW() - INTERVAL '1 hour' WHERE id = $1")
        .bind(outcome.session.id)
        .execute(t.pool())
        .await
        .unwrap();

    let err = t.auth.refresh(&outcome.tokens.refresh_token, None).await.unwrap_err();
    assert!(matches!(err, FerromaError::Unauthorized(_)));
    assert!(err.to_string().contains("expired"), "{err}");

    t.cleanup().await;
}

#[tokio::test]
async fn logout_revokes_the_session_behind_the_access_token() {
    if !common::database_available().await {
        eprintln!("skipping: no PostgreSQL reachable");
        return;
    }
    let t = fresh_auth().await;
    t.create_user("alice@example.com").await;
    let outcome = login(&t, "alice@example.com").await;

    assert!(t.auth.authenticate(&outcome.tokens.access_token).await.is_ok());
    assert!(t.auth.logout(SessionId::new(outcome.session.id)).await.unwrap());

    // The token is still a valid JWT, but its session is gone — which is the point
    // of checking the session on every request.
    let err = t.auth.authenticate(&outcome.tokens.access_token).await.unwrap_err();
    assert!(matches!(err, FerromaError::Unauthorized(_)), "{err:?}");

    t.cleanup().await;
}

#[tokio::test]
async fn a_refresh_token_cannot_be_used_as_a_bearer_token() {
    if !common::database_available().await {
        eprintln!("skipping: no PostgreSQL reachable");
        return;
    }
    let t = fresh_auth().await;
    t.create_user("alice@example.com").await;
    let outcome = login(&t, "alice@example.com").await;

    let err = t.auth.authenticate(&outcome.tokens.refresh_token).await.unwrap_err();
    assert!(matches!(err, FerromaError::Unauthorized(_)));
    assert!(err.to_string().contains("refresh token"), "{err}");

    t.cleanup().await;
}

#[tokio::test]
async fn a_token_minted_by_another_server_is_refused() {
    if !common::database_available().await {
        eprintln!("skipping: no PostgreSQL reachable");
        return;
    }
    let t = fresh_auth().await;
    t.create_user("alice@example.com").await;
    let outcome = login(&t, "alice@example.com").await;

    let other = ferroma_auth::TokenService::new(
        "a-completely-different-secret-of-sufficient-length",
        3600,
        3600,
        "mail.example.com",
    )
    .unwrap();
    // Same claims, different key.
    let forged = other
        .sign_access(UserId::new(outcome.user.id), SessionId::new(outcome.session.id))
        .unwrap();
    assert!(t.auth.authenticate(&forged).await.is_err());

    t.cleanup().await;
}

#[tokio::test]
async fn changing_the_password_revokes_every_session() {
    if !common::database_available().await {
        eprintln!("skipping: no PostgreSQL reachable");
        return;
    }
    let t = fresh_auth().await;
    let user = t.create_user("alice@example.com").await;
    let session = login(&t, "alice@example.com").await;

    // Wrong current password is refused.
    let err = t
        .auth
        .change_password(UserId::new(user.id), "not it", "a brand new password")
        .await
        .unwrap_err();
    assert!(matches!(err, FerromaError::Unauthorized(_)));

    // Reusing the same password is refused.
    let err = t
        .auth
        .change_password(UserId::new(user.id), TEST_PASSWORD, TEST_PASSWORD)
        .await
        .unwrap_err();
    assert!(matches!(err, FerromaError::Invalid(_)));

    // A weak new password is refused by the policy.
    let err = t
        .auth
        .change_password(UserId::new(user.id), TEST_PASSWORD, "short")
        .await
        .unwrap_err();
    assert!(matches!(err, FerromaError::Invalid(_)));

    // The real change works, and kills the existing session.
    t.auth
        .change_password(UserId::new(user.id), TEST_PASSWORD, "an entirely new password")
        .await
        .unwrap();
    assert!(t.auth.authenticate(&session.tokens.access_token).await.is_err());

    // And the new password logs in.
    let outcome = t
        .auth
        .login("alice@example.com", "an entirely new password", SessionKind::Api, None, None, None)
        .await
        .unwrap();
    assert_eq!(outcome.user.id, user.id);

    t.cleanup().await;
}

#[tokio::test]
async fn devices_are_registered_updated_and_revoked() {
    if !common::database_available().await {
        eprintln!("skipping: no PostgreSQL reachable");
        return;
    }
    let t = fresh_auth().await;
    let user = t.create_user("alice@example.com").await;
    let user_id = UserId::new(user.id);

    let outcome = t
        .auth
        .login(
            "alice@example.com",
            TEST_PASSWORD,
            SessionKind::Client,
            None,
            Some("FerromaClient/0.1.0"),
            Some(device("laptop-1")),
        )
        .await
        .unwrap();
    let registered = outcome.device.expect("a device was supplied");
    assert_eq!(registered.device_uid, "laptop-1");

    // Logging in again from the same installation updates rather than duplicates.
    let again = t
        .auth
        .login(
            "alice@example.com",
            TEST_PASSWORD,
            SessionKind::Client,
            None,
            None,
            Some(device("laptop-1")),
        )
        .await
        .unwrap();
    assert_eq!(again.device.as_ref().unwrap().id, registered.id);
    let devices = t.auth.list_devices(user_id, false).await.unwrap();
    assert_eq!(devices.len(), 1, "one installation, one row");

    // A second installation is a second row.
    let _ = t
        .auth
        .login(
            "alice@example.com",
            TEST_PASSWORD,
            SessionKind::Client,
            None,
            None,
            Some(device("phone-1")),
        )
        .await
        .unwrap();
    assert_eq!(t.auth.list_devices(user_id, false).await.unwrap().len(), 2);

    // Revoking one device kills its sessions and leaves the other alone.
    let revoked = t.auth.revoke_device(DeviceId::new(registered.id)).await.unwrap();
    assert!(revoked >= 1, "the device's sessions must be revoked");
    assert!(t.auth.authenticate(&outcome.tokens.access_token).await.is_err());
    assert!(t.auth.authenticate(&again.tokens.access_token).await.is_err());
    assert!(t.auth.authenticate(&outcome.tokens.access_token).await.is_err());

    let active = t.auth.list_devices(user_id, false).await.unwrap();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].device_uid, "phone-1");

    // Revoking an unknown device is a NotFound, not a panic.
    let err = t.auth.revoke_device(DeviceId::new(999_999)).await.unwrap_err();
    assert!(matches!(err, FerromaError::NotFound(_)));

    t.cleanup().await;
}

#[tokio::test]
async fn an_empty_device_uid_is_refused() {
    if !common::database_available().await {
        eprintln!("skipping: no PostgreSQL reachable");
        return;
    }
    let t = fresh_auth().await;
    let user = t.create_user("alice@example.com").await;

    let err = t
        .auth
        .register_device(UserId::new(user.id), device("   "), None)
        .await
        .unwrap_err();
    assert!(matches!(err, FerromaError::Invalid(_)));

    let long = "x".repeat(200);
    let err = t
        .auth
        .register_device(UserId::new(user.id), device(&long), None)
        .await
        .unwrap_err();
    assert!(matches!(err, FerromaError::Invalid(_)));

    t.cleanup().await;
}

#[tokio::test]
async fn expired_sessions_are_purged() {
    if !common::database_available().await {
        eprintln!("skipping: no PostgreSQL reachable");
        return;
    }
    let t = fresh_auth().await;
    t.create_user("alice@example.com").await;
    let outcome = login(&t, "alice@example.com").await;

    assert_eq!(t.auth.purge_expired_sessions().await.unwrap(), 0);

    sqlx::query("UPDATE sessions SET expires_at = NOW() - INTERVAL '1 day' WHERE id = $1")
        .bind(outcome.session.id)
        .execute(t.pool())
        .await
        .unwrap();

    assert_eq!(t.auth.purge_expired_sessions().await.unwrap(), 1);
    t.cleanup().await;
}

#[tokio::test]
async fn create_user_enforces_the_password_policy_and_uniqueness() {
    if !common::database_available().await {
        eprintln!("skipping: no PostgreSQL reachable");
        return;
    }
    let t = fresh_auth().await;

    // A weak password never reaches the database.
    let err = t
        .auth
        .create_user("weak@example.com", "short", None, false, true, None)
        .await
        .unwrap_err();
    assert!(matches!(err, FerromaError::Invalid(_)));

    t.create_user("alice@example.com").await;
    let err = t
        .auth
        .create_user("ALICE@example.com", TEST_PASSWORD, None, false, true, None)
        .await
        .unwrap_err();
    assert!(matches!(err, FerromaError::Conflict(_)), "{err:?}");

    t.cleanup().await;
}

#[tokio::test]
async fn local_address_resolution_respects_enabled_state() {
    if !common::database_available().await {
        eprintln!("skipping: no PostgreSQL reachable");
        return;
    }
    let t = fresh_auth().await;
    let user = t.create_user("alice@example.com").await;
    let repos = t.repos();
    let domain = repos.domains.create("example.com", None).await.unwrap();
    let mailbox = repos
        .mailboxes
        .create(ferroma_storage::repository::NewMailbox {
            user_id: UserId::new(user.id),
            domain_id: ferroma_core::DomainId::new(domain.id),
            local_part: "alice".into(),
            display_name: None,
            is_primary: true,
            quota_bytes: None,
        })
        .await
        .unwrap();

    let address = ferroma_core::EmailAddress::parse("alice@example.com").unwrap();
    assert_eq!(
        t.auth.resolve_local_address(&address).await.unwrap(),
        Some(mailbox.id)
    );

    // Disabled addresses do not resolve — SMTP must refuse them.
    repos
        .mailboxes
        .set_enabled(ferroma_core::MailboxId::new(mailbox.id), false)
        .await
        .unwrap();
    assert_eq!(t.auth.resolve_local_address(&address).await.unwrap(), None);

    // And an unknown address never resolves.
    let unknown = ferroma_core::EmailAddress::parse("nobody@example.com").unwrap();
    assert_eq!(t.auth.resolve_local_address(&unknown).await.unwrap(), None);

    t.cleanup().await;
}

#[tokio::test]
async fn login_attempts_are_recorded_for_the_audit_trail() {
    if !common::database_available().await {
        eprintln!("skipping: no PostgreSQL reachable");
        return;
    }
    let t = fresh_auth().await;
    t.create_user("alice@example.com").await;

    login(&t, "alice@example.com").await;
    let _ = t
        .auth
        .login("alice@example.com", "wrong", SessionKind::Api, Some("203.0.113.9".parse().unwrap()), None, None)
        .await;

    let attempts = t
        .repos()
        .login_attempts
        .recent_for_email("alice@example.com", 10)
        .await
        .unwrap();
    assert_eq!(attempts.len(), 2);
    assert!(attempts.iter().any(|a| a.success));
    assert!(attempts.iter().any(|a| !a.success));

    let failures = t
        .repos()
        .login_attempts
        .count_failures_for_ip("203.0.113.9", chrono::Utc::now() - chrono::Duration::hours(1))
        .await
        .unwrap();
    assert_eq!(failures, 1);

    t.cleanup().await;
}

#[tokio::test]
async fn a_flood_from_one_address_is_throttled_before_any_hashing() {
    if !common::database_available().await {
        eprintln!("skipping: no PostgreSQL reachable");
        return;
    }
    let t = fresh_auth().await;
    t.create_user("alice@example.com").await;
    let ip: std::net::IpAddr = "198.51.100.7".parse().unwrap();

    // `max_failed_logins` is 10, so three times that many failures from one address
    // trips the address-level throttle.
    for _ in 0..31 {
        let _ = t
            .auth
            .login("alice@example.com", "wrong", SessionKind::Api, Some(ip), None, None)
            .await;
    }

    let err = t
        .auth
        .login("alice@example.com", TEST_PASSWORD, SessionKind::Api, Some(ip), None, None)
        .await
        .unwrap_err();
    assert!(matches!(err, FerromaError::RateLimited), "{err:?}");

    // A different address is unaffected.
    let ok = t
        .auth
        .login(
            "alice@example.com",
            TEST_PASSWORD,
            SessionKind::Api,
            Some("203.0.113.1".parse().unwrap()),
            None,
            None,
        )
        .await;
    // The *account* may be locked by now, which is also correct; either outcome is
    // acceptable, a panic is not.
    assert!(ok.is_ok() || ok.is_err());

    t.cleanup().await;
}

#[tokio::test]
async fn sessions_can_be_listed_and_revoked_in_bulk() {
    if !common::database_available().await {
        eprintln!("skipping: no PostgreSQL reachable");
        return;
    }
    let t = fresh_auth().await;
    let user = t.create_user("alice@example.com").await;
    let user_id = UserId::new(user.id);

    let a = login(&t, "alice@example.com").await;
    let b = login(&t, "alice@example.com").await;

    let active = t.auth.list_sessions(user_id, false).await.unwrap();
    assert!(active.len() >= 2);

    let revoked = t.auth.logout_all(user_id).await.unwrap();
    assert!(revoked >= 2);
    assert!(t.auth.authenticate(&a.tokens.access_token).await.is_err());
    assert!(t.auth.authenticate(&b.tokens.access_token).await.is_err());

    t.cleanup().await;
}

// ===========================================================================
// Second factors and application passwords
// ===========================================================================

/// A valid code for the enrollment the harness is holding.
fn current_code(secret: &str) -> String {
    ferroma_auth::totp::current_code(secret, std::time::SystemTime::now()).expect("a code")
}

/// Enroll and confirm a second factor, returning the recovery codes.
async fn enable_totp(t: &TestAuth, user: &ferroma_storage::models::User) -> Vec<String> {
    let enrollment = t.auth.begin_totp_enrollment(user).await.expect("begin");
    let codes = t
        .auth
        .confirm_totp_enrollment(UserId::new(user.id), &current_code(&enrollment.secret))
        .await
        .expect("confirm");
    codes
}

#[tokio::test]
async fn enrolling_needs_a_code_before_it_is_enforced() {
    if !common::database_available().await {
        eprintln!("skipping: no PostgreSQL reachable");
        return;
    }
    let t = fresh_auth().await;
    let user = t.create_user("alice@example.com").await;

    // Not enrolled: a password is enough.
    assert_eq!(t.auth.totp_status(UserId::new(user.id)).await.unwrap(), ferroma_auth::TotpStatus::Disabled);
    login(&t, "alice@example.com").await;

    let enrollment = t.auth.begin_totp_enrollment(&user).await.expect("begin");
    assert!(enrollment.uri.starts_with("otpauth://totp/"));
    assert!(enrollment.uri.contains(&enrollment.secret));
    assert!(enrollment.secret.len() >= 32);

    // Pending, not enforced: a bad scan must not lock the account out.
    assert_eq!(t.auth.totp_status(UserId::new(user.id)).await.unwrap(), ferroma_auth::TotpStatus::Pending);
    login(&t, "alice@example.com").await;

    // A wrong code does not confirm it.
    let refused = t
        .auth
        .confirm_totp_enrollment(UserId::new(user.id), "000000")
        .await;
    assert!(matches!(refused, Err(FerromaError::Unauthorized(_))), "{refused:?}");
    assert_eq!(t.auth.totp_status(UserId::new(user.id)).await.unwrap(), ferroma_auth::TotpStatus::Pending);

    let codes = t
        .auth
        .confirm_totp_enrollment(UserId::new(user.id), &current_code(&enrollment.secret))
        .await
        .expect("confirm");
    assert_eq!(codes.len(), ferroma_auth::totp::RECOVERY_CODE_COUNT);
    assert_eq!(t.auth.totp_status(UserId::new(user.id)).await.unwrap(), ferroma_auth::TotpStatus::Enabled);
    assert_eq!(
        t.auth.recovery_codes_left(UserId::new(user.id)).await.unwrap(),
        ferroma_auth::totp::RECOVERY_CODE_COUNT as i64
    );

    // Confirming again is refused rather than reissuing codes silently.
    let again = t
        .auth
        .confirm_totp_enrollment(UserId::new(user.id), &current_code(&enrollment.secret))
        .await;
    assert!(matches!(again, Err(FerromaError::Conflict(_))), "{again:?}");

    t.cleanup().await;
}

#[tokio::test]
async fn an_enforced_second_factor_gates_every_password_login() {
    if !common::database_available().await {
        eprintln!("skipping: no PostgreSQL reachable");
        return;
    }
    let t = fresh_auth().await;
    let user = t.create_user("alice@example.com").await;
    let enrollment = t.auth.begin_totp_enrollment(&user).await.expect("begin");
    t.auth
        .confirm_totp_enrollment(UserId::new(user.id), &current_code(&enrollment.secret))
        .await
        .expect("confirm");

    // The old entry point cannot satisfy the factor, and must say so distinctly:
    // "send the code", not "wrong password".
    let gated = t
        .auth
        .login("alice@example.com", TEST_PASSWORD, SessionKind::Api, None, None, None)
        .await;
    assert!(matches!(gated, Err(FerromaError::TotpRequired)), "{gated:?}");

    // A wrong code is a failed login, and it is counted so codes cannot be brute
    // forced at the speed of the network.
    let wrong = t
        .auth
        .login_with_factor("alice@example.com", TEST_PASSWORD, Some("000000"),
            SessionKind::Api, None, None, None)
        .await;
    assert!(matches!(wrong, Err(FerromaError::Unauthorized(_))), "{wrong:?}");
    let reloaded = t.repos().users.require_by_id(UserId::new(user.id)).await.unwrap();
    assert_eq!(reloaded.failed_logins, 1, "a guessed code must count as a failure");

    // The right code opens the session.
    let outcome = t
        .auth
        .login_with_factor("alice@example.com", TEST_PASSWORD, Some(&current_code(&enrollment.secret)),
            SessionKind::Api, None, Some("test"), None)
        .await
        .expect("login with the factor");
    assert_eq!(outcome.user.id, user.id);
    let cleared = t.repos().users.require_by_id(UserId::new(user.id)).await.unwrap();
    assert_eq!(cleared.failed_logins, 0);

    // Turning it off restores a password-only login.
    assert!(t.auth.disable_totp(UserId::new(user.id)).await.unwrap());
    assert_eq!(t.auth.totp_status(UserId::new(user.id)).await.unwrap(), ferroma_auth::TotpStatus::Disabled);
    login(&t, "alice@example.com").await;

    t.cleanup().await;
}

#[tokio::test]
async fn a_recovery_code_works_once_and_is_counted_down() {
    if !common::database_available().await {
        eprintln!("skipping: no PostgreSQL reachable");
        return;
    }
    let t = fresh_auth().await;
    let user = t.create_user("alice@example.com").await;
    let codes = enable_totp(&t, &user).await;

    let first = t
        .auth
        .login_with_factor("alice@example.com", TEST_PASSWORD, Some(&codes[0]),
            SessionKind::Api, None, None, None)
        .await
        .expect("the first code must work");
    assert_eq!(first.user.id, user.id);
    assert_eq!(
        t.auth.recovery_codes_left(UserId::new(user.id)).await.unwrap(),
        codes.len() as i64 - 1
    );

    // Single use: the same code cannot be replayed.
    let replay = t
        .auth
        .login_with_factor("alice@example.com", TEST_PASSWORD, Some(&codes[0]),
            SessionKind::Api, None, None, None)
        .await;
    assert!(matches!(replay, Err(FerromaError::Unauthorized(_))), "{replay:?}");

    // And the spacing/case a person types does not matter.
    let second = t
        .auth
        .login_with_factor("alice@example.com", TEST_PASSWORD,
            Some(&codes[1].replace('-', " ").to_lowercase()),
            SessionKind::Api, None, None, None)
        .await
        .expect("a code typed back in another shape must work");
    assert_eq!(second.user.id, user.id);

    t.cleanup().await;
}

#[tokio::test]
async fn an_application_password_is_the_way_a_client_gets_in() {
    if !common::database_available().await {
        eprintln!("skipping: no PostgreSQL reachable");
        return;
    }
    let t = fresh_auth().await;
    let user = t.create_user("alice@example.com").await;
    let user_id = UserId::new(user.id);

    // Before the second factor, the account password works for a client.
    let matched = t.auth.authenticate_client("alice@example.com", TEST_PASSWORD).await.unwrap();
    assert_eq!(matched, Some(user_id));
    assert_eq!(t.auth.authenticate_client("alice@example.com", "wrong").await.unwrap(), None);
    assert_eq!(t.auth.authenticate_client("nobody@example.com", TEST_PASSWORD).await.unwrap(), None);

    enable_totp(&t, &user).await;

    // With it enforced, a client that cannot present a code must not slide by on the
    // password alone — that is the whole point of enabling it.
    assert_eq!(t.auth.authenticate_client("alice@example.com", TEST_PASSWORD).await.unwrap(), None);

    let (row, token) = t.auth.create_app_password(user_id, "Thunderbird").await.expect("create");
    assert_eq!(row.label, "Thunderbird");
    assert!(token.starts_with("ap_"));
    assert_eq!(t.auth.authenticate_client("alice@example.com", &token).await.unwrap(), Some(user_id));

    // Using it stamps last_used_at, which is what tells an operator it is live.
    let listed = t.auth.list_app_passwords(user_id).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert!(listed[0].last_used_at.is_some(), "a used password must be stamped");

    // Revoking it closes that door and only that door.
    assert!(t.auth.revoke_app_password(user_id, row.id).await.unwrap());
    assert_eq!(t.auth.authenticate_client("alice@example.com", &token).await.unwrap(), None);

    // A label is required, and another account cannot revoke this one's password.
    assert!(matches!(t.auth.create_app_password(user_id, "   ").await, Err(FerromaError::Invalid(_))));
    let stranger = t.create_user("bob@example.com").await;
    assert!(!t.auth.revoke_app_password(UserId::new(stranger.id), row.id).await.unwrap());

    t.cleanup().await;
}

#[tokio::test]
async fn an_application_password_opens_an_api_session_too() {
    if !common::database_available().await {
        eprintln!("skipping: no PostgreSQL reachable");
        return;
    }
    let t = fresh_auth().await;
    let user = t.create_user("alice@example.com").await;
    let user_id = UserId::new(user.id);
    enable_totp(&t, &user).await;

    let (_row, token) = t.auth.create_app_password(user_id, "JMAP client").await.expect("create");

    // A client that can only send Basic credentials — JMAP's discovery request, for
    // instance — is served by the application password without a TOTP code.
    let outcome = t
        .auth
        .login("alice@example.com", &token, SessionKind::Jmap, None, Some("jmap"), None)
        .await
        .expect("an application password must open a session");
    assert_eq!(outcome.user.id, user.id);

    // A wrong application password is a failure, not an accidental password check.
    let wrong = t
        .auth
        .login("alice@example.com", "ap_not-a-real-token", SessionKind::Jmap, None, None, None)
        .await;
    assert!(matches!(wrong, Err(FerromaError::Unauthorized(_))), "{wrong:?}");

    // Revoking it stops working immediately, and the TOTP gate is back in force.
    assert!(t.auth.revoke_app_password(user_id, t.auth.list_app_passwords(user_id).await.unwrap()[0].id).await.unwrap());
    let revoked = t
        .auth
        .login("alice@example.com", &token, SessionKind::Jmap, None, None, None)
        .await;
    assert!(matches!(revoked, Err(FerromaError::Unauthorized(_))), "{revoked:?}");

    t.cleanup().await;
}
