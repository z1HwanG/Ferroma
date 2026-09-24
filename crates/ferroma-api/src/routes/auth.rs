//! `docs/api.md` §3 — authentication on the management surface.
//!
//! # Two flows, one endpoint
//!
//! `POST /auth/login` answers a *browser* and a *script* differently, and the
//! difference is `device_name`:
//!
//! * **with** `device_name` the caller is an automation or an app that will hold the
//!   tokens itself; the response carries them and no cookie is set;
//! * **without** it the caller is Webmail, which cannot keep a token anywhere safe, so
//!   the server also sets an `HttpOnly` `ferroma_session` cookie. That cookie is an
//!   opaque session secret — [`ferroma_auth`] mints it and stores only its SHA-256 —
//!   and [`crate::extract`] resolves it on every later request.
//!
//! # The cookie's attributes
//!
//! `HttpOnly` (no script can read it), `SameSite=Lax` (a cross-site POST cannot use
//! it, but a link from an email still works), `Path=/`, and `Secure` when
//! `api.secure_cookies` is on — which it must be in production, because without it the
//! cookie travels in clear text over any plain-HTTP hop.

use axum::extract::State;
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use ferroma_auth::SessionKind;
use ferroma_core::{FerromaError, UserId};
use serde::{Deserialize, Serialize};

use crate::error::ApiError;
use crate::extract::{AuthUser, SESSION_COOKIE};
use crate::routes::mail::shapes::{MeResponse, UserResponse};
use crate::state::AppState;

/// The `POST /api/v1/auth/login` body.
#[derive(Debug, Clone, Deserialize)]
pub struct LoginRequest {
    /// The login address.
    pub email: String,
    /// The password.
    pub password: String,
    /// Names the device. When present, the flow is a token flow and no cookie is set.
    pub device_name: Option<String>,
    /// A TOTP code or a recovery code, when the account enforces a second factor.
    ///
    /// Omitted on the first attempt. The answer is `401 totp_required`, which means
    /// "send the code" — not "the password was wrong", so a client must not retry
    /// the password.
    pub totp: Option<String>,
}

/// The token pair every login and refresh answers with.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenResponse {
    /// The short-lived bearer token.
    pub access_token: String,
    /// The single-use refresh token.
    pub refresh_token: String,
    /// Always `"Bearer"`.
    pub token_type: String,
    /// Access-token lifetime, in seconds.
    pub expires_in: u64,
    /// The authenticated account.
    pub user: UserResponse,
}

/// The `POST /api/v1/auth/refresh` body.
#[derive(Debug, Clone, Deserialize)]
pub struct RefreshRequest {
    /// The refresh token to rotate.
    pub refresh_token: String,
}

/// The `POST /api/v1/auth/jmap-token` body.
///
/// A JMAP token is deliberately minted from an already authenticated Ferroma session;
/// it is a distinct, revocable session rather than a second password database.
#[derive(Debug, Clone, Deserialize)]
pub struct JmapTokenRequest {
    /// A human-readable device name for the Admin session list.
    pub device_name: String,
}

/// The password grant for a separately revocable JMAP bearer-token session.
#[derive(Debug, Clone, Deserialize)]
pub struct JmapLoginRequest {
    /// The account login address.
    pub email: String,
    /// The account password.
    pub password: String,
    /// A human-readable name for this JMAP client installation.
    pub device_name: String,
    /// A TOTP code or a recovery code, when the account enforces a second factor.
    /// A JMAP client that cannot be asked for one uses an application password as
    /// its `password` instead.
    pub totp: Option<String>,
}

/// The `POST /api/v1/auth/password` body.
#[derive(Debug, Clone, Deserialize)]
pub struct PasswordChangeRequest {
    /// The password in use.
    pub current_password: String,
    /// The replacement.
    pub new_password: String,
}

/// Build the `Set-Cookie` value for a session secret.
///
/// Kept as a free function so a unit test can assert every attribute without a live
/// server: this is one of the few places where a mistake is a security bug rather than
/// a bug report.
pub fn session_cookie_value(secret: &str, secure: bool, max_age_secs: u64) -> String {
    let mut value = format!(
        "{SESSION_COOKIE}={secret}; Path=/; HttpOnly; SameSite=Lax; Max-Age={max_age_secs}"
    );
    if secure {
        value.push_str("; Secure");
    }
    value
}

/// Build the `Set-Cookie` value that clears the session cookie.
pub fn cleared_cookie_value(secure: bool) -> String {
    let mut value = format!("{SESSION_COOKIE}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0");
    if secure {
        value.push_str("; Secure");
    }
    value
}

/// The peer address of a request, when it can be trusted.
///
/// `api.trust_proxy_headers` decides whether `X-Forwarded-For` is believed: a server
/// not behind a proxy must ignore it, because a client can set it to anything and the
/// value feeds login throttling.
pub fn peer_ip(state: &AppState, headers: &HeaderMap) -> Option<std::net::IpAddr> {
    if !state.config.api.trust_proxy_headers {
        return None;
    }
    proxied_ip(headers)
}

/// Read the client address out of the proxy headers a trusted proxy sets.
///
/// `X-Forwarded-For` may carry a chain; the first entry is the original client.
pub fn proxied_ip(headers: &HeaderMap) -> Option<std::net::IpAddr> {
    for name in ["x-forwarded-for", "x-real-ip"] {
        if let Some(raw) = headers.get(name).and_then(|value| value.to_str().ok()) {
            let first = raw.split(',').next().unwrap_or(raw).trim();
            if let Ok(ip) = first.parse::<std::net::IpAddr>() {
                return Some(ip);
            }
        }
    }
    None
}

/// `POST /api/v1/auth/login`
pub async fn login(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<LoginRequest>,
) -> Result<Response, ApiError> {
    let ip = peer_ip(&state, &headers);
    let user_agent = headers
        .get(header::USER_AGENT)
        .and_then(|value| value.to_str().ok());

    let browser_flow = request.device_name.is_none();
    let device = request.device_name.as_deref().map(|name| {
        ferroma_auth::DeviceInfo {
            // The management surface has no stable installation id; the name is what
            // the Admin panel's session list shows.
            device_uid: format!("web:{name}"),
            name: Some(name.to_string()),
            platform: None,
            client_version: None,
            protocol_version: None,
        }
    });

    let outcome = state
        .auth
        .login_with_factor(
            &request.email,
            &request.password,
            request.totp.as_deref(),
            SessionKind::Web,
            ip,
            user_agent,
            device,
        )
        .await
        .map_err(|err| match err {
            // The lockout window is the honest `Retry-After`.
            FerromaError::RateLimited => ApiError::new(FerromaError::RateLimited)
                .with_retry_after(state.config.limits.login_lockout_secs),
            other => ApiError::new(other),
        })?;

    let body = TokenResponse {
        access_token: outcome.tokens.access_token.clone(),
        refresh_token: outcome.tokens.refresh_token.clone(),
        token_type: "Bearer".to_string(),
        expires_in: outcome.tokens.expires_in,
        user: UserResponse::from_row(&outcome.user),
    };

    let mut response = Json(body).into_response();
    if browser_flow {
        // The cookie carries the *refresh* secret: it is the long-lived half, and it is
        // the one the session row stores a hash of.
        let cookie = session_cookie_value(
            &outcome.tokens.refresh_token,
            state.config.api.secure_cookies,
            state.config.api.session_ttl_secs,
        );
        if let Ok(value) = HeaderValue::from_str(&cookie) {
            response.headers_mut().append(header::SET_COOKIE, value);
        }
    }

    Ok(response)
}

/// `POST /api/v1/jmap/auth/token`.
///
/// This password grant creates a separately revocable JMAP bearer-token session. It
/// does not set a browser cookie and it never exposes a password to another protocol.
pub async fn jmap_login(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<JmapLoginRequest>,
) -> Result<Json<TokenResponse>, ApiError> {
    if !state.listeners.states().jmap.enabled {
        return Err(ApiError::new(FerromaError::NotFound(
            "the JMAP service is disabled by the administrator".to_string(),
        )));
    }
    let device_name = request.device_name.trim();
    if device_name.is_empty() || device_name.len() > 255 {
        return Err(ApiError::new(FerromaError::Invalid(
            "device_name must contain 1 to 255 characters".to_string(),
        )));
    }
    let ip = peer_ip(&state, &headers);
    let user_agent = headers
        .get(header::USER_AGENT)
        .and_then(|value| value.to_str().ok());
    let outcome = state
        .auth
        .login_with_factor(
            &request.email,
            &request.password,
            request.totp.as_deref(),
            SessionKind::Jmap,
            ip,
            user_agent,
            Some(ferroma_auth::DeviceInfo {
                device_uid: format!("jmap:{device_name}"),
                name: Some(device_name.to_string()),
                platform: None,
                client_version: None,
                protocol_version: None,
            }),
        )
        .await
        .map_err(|err| match err {
            FerromaError::RateLimited => ApiError::new(FerromaError::RateLimited)
                .with_retry_after(state.config.limits.login_lockout_secs),
            other => ApiError::new(other),
        })?;
    Ok(Json(TokenResponse {
        access_token: outcome.tokens.access_token,
        refresh_token: outcome.tokens.refresh_token,
        token_type: "Bearer".to_string(),
        expires_in: outcome.tokens.expires_in,
        user: UserResponse::from_row(&outcome.user),
    }))
}

/// `POST /api/v1/auth/jmap-token`.
///
/// The returned bearer token is accepted only by the JMAP endpoints. Its refresh token
/// uses the existing rotating-session mechanism, so revocation and expiry remain shared
/// with every other Ferroma authentication surface.
pub async fn create_jmap_token(
    State(state): State<AppState>,
    user: AuthUser,
    Json(request): Json<JmapTokenRequest>,
) -> Result<Json<TokenResponse>, ApiError> {
    let name = request.device_name.trim();
    if name.is_empty() || name.len() > 255 {
        return Err(ApiError::new(FerromaError::Invalid(
            "device_name must contain 1 to 255 characters".to_string(),
        )));
    }
    let (_session, tokens) = state
        .auth
        .open_session(user.user(), SessionKind::Jmap, None, None, Some(name))
        .await
        .map_err(ApiError::from)?;
    Ok(Json(TokenResponse {
        access_token: tokens.access_token,
        refresh_token: tokens.refresh_token,
        token_type: "Bearer".to_string(),
        expires_in: tokens.expires_in,
        user: UserResponse::from_row(user.user()),
    }))
}

/// `POST /api/v1/auth/refresh`
pub async fn refresh(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<RefreshRequest>,
) -> Result<Response, ApiError> {
    let ip = peer_ip(&state, &headers);
    let tokens = state
        .auth
        .refresh(&request.refresh_token, ip)
        .await
        .map_err(ApiError::from)?;

    // The refreshed session must report the same account, so read it back.
    let session = state
        .repos
        .sessions
        .find_by_id(tokens.session_id)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(|| {
            ApiError::new(FerromaError::Unauthorized(
                "the refreshed session disappeared".to_string(),
            ))
        })?;
    let user = state
        .repos
        .users
        .require_by_id(UserId::new(session.user_id))
        .await
        .map_err(ApiError::from)?;

    let body = TokenResponse {
        access_token: tokens.access_token.clone(),
        refresh_token: tokens.refresh_token.clone(),
        token_type: "Bearer".to_string(),
        expires_in: tokens.expires_in,
        user: UserResponse::from_row(&user),
    };

    let mut response = Json(body).into_response();
    let cookie = session_cookie_value(
        &tokens.refresh_token,
        state.config.api.secure_cookies,
        state.config.api.session_ttl_secs,
    );
    if let Ok(value) = HeaderValue::from_str(&cookie) {
        response.headers_mut().append(header::SET_COOKIE, value);
    }
    Ok(response)
}

/// `POST /api/v1/auth/logout` — revokes the session and clears the cookie.
pub async fn logout(State(state): State<AppState>, user: AuthUser) -> Result<Response, ApiError> {
    state
        .auth
        .logout(user.session_id())
        .await
        .map_err(ApiError::from)?;

    let mut response = StatusCode::NO_CONTENT.into_response();
    let cookie = cleared_cookie_value(state.config.api.secure_cookies);
    if let Ok(value) = HeaderValue::from_str(&cookie) {
        response.headers_mut().append(header::SET_COOKIE, value);
    }
    Ok(response)
}

/// `GET /api/v1/auth/me`
pub async fn me(
    State(state): State<AppState>,
    user: AuthUser,
) -> Result<Json<MeResponse>, ApiError> {
    let rows = state
        .repos
        .mailboxes
        .list_by_user(user.user_id())
        .await
        .map_err(ApiError::from)?;

    // Full mailbox records, with their usage: the Webmail reads each address's quota and
    // current size from these, and the reduced `AddressBrief` carries neither.
    let mut mailboxes = Vec::with_capacity(rows.len());
    for row in rows {
        let domain = crate::routes::mail::store::domain_name(&state.repos, row.domain_id).await?;
        mailboxes
            .push(crate::routes::mail::mailboxes::mailbox_response(&state, &row, &domain).await);
    }

    Ok(Json(MeResponse {
        id: user.user().id,
        email: user.user().email.clone(),
        display_name: user.user().display_name.clone(),
        is_admin: user.user().is_admin,
        quota_bytes: user.user().quota_bytes,
        used_bytes: user.user().used_bytes,
        mailboxes,
    }))
}

/// `POST /api/v1/auth/password` — revokes every other session.
pub async fn change_password(
    State(state): State<AppState>,
    user: AuthUser,
    Json(request): Json<PasswordChangeRequest>,
) -> Result<StatusCode, ApiError> {
    state
        .auth
        .change_password(
            user.user_id(),
            &request.current_password,
            &request.new_password,
        )
        .await
        .map_err(ApiError::from)?;

    // `change_password` revokes *every* session, including the caller's. That is the
    // documented behaviour ("revokes every other session") taken one step further: the
    // caller must log in again, which is the safe direction to be wrong in.
    tracing::info!(user_id = user.user_id().get(), "password changed");
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_secure_cookie_carries_every_defensive_attribute() {
        let cookie = session_cookie_value("rt_secret", true, 86_400);
        assert!(cookie.starts_with("ferroma_session=rt_secret;"), "{cookie}");
        assert!(cookie.contains("Path=/"), "{cookie}");
        assert!(cookie.contains("HttpOnly"), "{cookie}");
        assert!(cookie.contains("SameSite=Lax"), "{cookie}");
        assert!(cookie.contains("Max-Age=86400"), "{cookie}");
        assert!(cookie.contains("; Secure"), "{cookie}");
    }

    #[test]
    fn a_development_cookie_omits_secure_only() {
        let cookie = session_cookie_value("rt_secret", false, 3600);
        assert!(!cookie.contains("Secure"), "{cookie}");
        // Every other attribute is unconditional.
        assert!(cookie.contains("HttpOnly"), "{cookie}");
        assert!(cookie.contains("SameSite=Lax"), "{cookie}");
        assert!(cookie.contains("Max-Age=3600"), "{cookie}");
    }

    #[test]
    fn clearing_a_cookie_expires_it_immediately() {
        let cookie = cleared_cookie_value(false);
        assert!(cookie.starts_with("ferroma_session=;"), "{cookie}");
        assert!(cookie.contains("Max-Age=0"), "{cookie}");
        assert!(cookie.contains("HttpOnly"), "{cookie}");
        assert!(!cookie.contains("Secure"), "{cookie}");

        let secure = cleared_cookie_value(true);
        assert!(secure.contains("; Secure"), "{secure}");
    }

    #[test]
    fn every_cookie_never_carries_a_domain_attribute() {
        // A `Domain` attribute would widen the cookie's scope to subdomains, which is
        // never what a mail session wants.
        for cookie in [
            session_cookie_value("x", true, 1),
            session_cookie_value("x", false, 1),
            cleared_cookie_value(true),
            cleared_cookie_value(false),
        ] {
            assert!(!cookie.to_ascii_lowercase().contains("domain="), "{cookie}");
        }
    }

    #[test]
    fn the_token_response_shape_matches_the_documentation() {
        let body = TokenResponse {
            access_token: "eyJ".into(),
            refresh_token: "rt_x".into(),
            token_type: "Bearer".into(),
            expires_in: 3600,
            user: UserResponse {
                id: 7,
                email: "alice@example.com".into(),
                display_name: Some("Alice".into()),
                enabled: true,
                is_admin: false,
                quota_bytes: 1_073_741_824,
                used_bytes: 52_428_800,
                last_login_at: None,
                created_at: chrono::Utc::now(),
                mailboxes: None,
            },
        };
        let json = serde_json::to_value(&body).expect("must serialise");
        assert_eq!(json["token_type"], "Bearer");
        assert_eq!(json["expires_in"], 3600);
        assert_eq!(json["user"]["id"], 7);
        assert_eq!(json["user"]["quota_bytes"], 1_073_741_824);
        assert!(json.get("session_id").is_none(), "not part of the contract");
    }

    #[test]
    fn the_login_body_deserialises_with_and_without_a_device_name() {
        let request: LoginRequest = serde_json::from_value(serde_json::json!({
            "email": "alice@example.com",
            "password": "hunter2"
        }))
        .expect("must deserialise");
        assert!(request.device_name.is_none());

        let request: LoginRequest = serde_json::from_value(serde_json::json!({
            "email": "alice@example.com",
            "password": "hunter2",
            "device_name": "Firefox on Linux"
        }))
        .expect("must deserialise");
        assert_eq!(request.device_name.as_deref(), Some("Firefox on Linux"));
    }

    #[test]
    fn the_password_body_requires_both_passwords() {
        assert!(
            serde_json::from_value::<PasswordChangeRequest>(serde_json::json!({
                "current_password": "a"
            }))
            .is_err()
        );
        let request: PasswordChangeRequest = serde_json::from_value(serde_json::json!({
            "current_password": "a",
            "new_password": "b"
        }))
        .expect("must deserialise");
        assert_eq!(request.new_password, "b");
    }

    #[test]
    fn proxy_headers_are_parsed_only_when_they_exist() {
        let mut headers = HeaderMap::new();
        assert_eq!(proxied_ip(&headers), None);

        headers.insert(
            "x-forwarded-for",
            HeaderValue::from_static("203.0.113.44, 10.0.0.1"),
        );
        assert_eq!(
            proxied_ip(&headers),
            Some("203.0.113.44".parse().expect("valid ip"))
        );

        headers.remove("x-forwarded-for");
        headers.insert("x-real-ip", HeaderValue::from_static("198.51.100.7"));
        assert_eq!(
            proxied_ip(&headers),
            Some("198.51.100.7".parse().expect("valid ip"))
        );

        // Garbage is ignored rather than trusted.
        headers.insert("x-real-ip", HeaderValue::from_static("not-an-ip"));
        assert_eq!(proxied_ip(&headers), None);

        // IPv6 in brackets is not a bare address; it is refused.
        headers.insert("x-real-ip", HeaderValue::from_static("[::1]"));
        assert_eq!(proxied_ip(&headers), None);
        headers.insert("x-real-ip", HeaderValue::from_static("::1"));
        assert_eq!(proxied_ip(&headers), Some("::1".parse().expect("valid ip")));
    }

    #[test]
    fn the_session_kind_for_the_management_surface_is_web() {
        assert_eq!(SessionKind::Web.as_str(), "web");
        assert!(SessionKind::Web.is_refreshable());
    }
}
