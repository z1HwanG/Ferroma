//! `docs/fcp.md` §1, §2 — client authentication and version negotiation.
//!
//! # Device registration
//!
//! `POST /client/auth/login` takes a `device` object (`device_uid`, `name`, `platform`,
//! `client_version`) and registers it. The `device_uid` is what makes the login
//! idempotent per installation: logging in twice from the same laptop updates one
//! `devices` row rather than creating a second, which is what makes "revoke this
//! device" mean something.
//!
//! # The account blob
//!
//! `GET /client/account` returns the *negotiated* values plus the limits and the
//! feature list, so a client can record what it agreed to and discover optional
//! capability without a version bump.

use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use ferroma_auth::SessionKind;
use ferroma_core::{FerromaError, UserId};
use serde::{Deserialize, Serialize};

use crate::error::ApiError;
use crate::extract::{header_string, ClientAuth, ClientInfo, PLATFORM_HEADER};
use crate::routes::auth::peer_ip;
use crate::routes::mail::shapes::{AddressBrief, UserResponse};
use crate::state::AppState;

/// The `device` object of a client login (`docs/fcp.md` §2).
#[derive(Debug, Clone, Deserialize)]
pub struct DeviceDescriptor {
    /// Stable, client-generated installation id.
    pub device_uid: String,
    /// Human name, e.g. "Alice's laptop".
    pub name: Option<String>,
    /// `windows`, `linux`, `macos`, `android` or `ios`.
    pub platform: Option<String>,
    /// The client's own version string.
    pub client_version: Option<String>,
}

/// The `POST /client/auth/login` body.
#[derive(Debug, Clone, Deserialize)]
pub struct ClientLoginRequest {
    /// The login address.
    pub email: String,
    /// The password.
    pub password: String,
    /// The installation, when the client identifies itself in the body.
    pub device: Option<DeviceDescriptor>,
    /// A bare installation id, for clients that send the rest in headers.
    pub device_uid: Option<String>,
}

/// The `POST /client/auth/login` response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientLoginResponse {
    /// The short-lived bearer token.
    pub access_token: String,
    /// The single-use refresh token.
    pub refresh_token: String,
    /// Always `"Bearer"`.
    pub token_type: String,
    /// Access-token lifetime, in seconds.
    pub expires_in: u64,
    /// The registered device.
    pub device_id: i64,
    /// The authenticated account.
    pub user: UserResponse,
}

/// The `POST /client/auth/refresh` body.
#[derive(Debug, Clone, Deserialize)]
pub struct ClientRefreshRequest {
    /// The refresh token to rotate.
    pub refresh_token: String,
    /// The installation the rotation belongs to.
    pub device_uid: Option<String>,
}

/// The `POST /client/auth/logout` body.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ClientLogoutRequest {
    /// The refresh token to revoke, when the client still holds it.
    pub refresh_token: Option<String>,
}

/// The `GET /client/account` response (`docs/fcp.md` §1).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountResponse {
    /// The account.
    pub user: AccountUser,
    /// The addresses it owns.
    pub mailboxes: Vec<AddressBrief>,
    /// The protocol version in use.
    pub protocol_version: u32,
    /// The oldest version this server still accepts.
    pub min_protocol_version: u32,
    /// The server's release version.
    pub server_version: String,
    /// The server's hostname.
    pub server_hostname: String,
    /// The numeric limits a client should respect.
    pub limits: AccountLimits,
    /// Optional capabilities, so a version bump is never needed to discover one.
    pub features: Vec<String>,
}

/// The account half of [`AccountResponse`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountUser {
    /// The user row id.
    pub id: i64,
    /// The login address.
    pub email: String,
    /// Human name.
    pub display_name: Option<String>,
}

/// The limits half of [`AccountResponse`].
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct AccountLimits {
    /// Largest message the server accepts.
    pub max_message_size: u64,
    /// Most recipients one message may carry.
    pub max_recipients: usize,
    /// Bytes per chunked attachment upload.
    pub attachment_chunk_size: u64,
    /// Changes returned per sync page.
    pub sync_page_size: usize,
}

impl AccountLimits {
    /// Read the limits out of configuration.
    pub fn from_config(config: &ferroma_core::Config) -> Self {
        AccountLimits {
            max_message_size: config.limits.max_message_size,
            max_recipients: config.limits.max_recipients,
            attachment_chunk_size: config.client.attachment_chunk_size,
            sync_page_size: config.client.sync_page_size,
        }
    }
}

/// The features this build advertises.
///
/// `docs/fcp.md` §1: `features` is how a client discovers optional capability without a
/// version bump, so the list only names things that are actually implemented.
pub const FEATURES: [&str; 6] = [
    "sync",
    "events",
    "drafts",
    "attachments",
    "devices",
    "search",
];

/// `POST /api/v1/client/auth/login`
pub async fn client_login(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<ClientLoginRequest>,
) -> Result<Response, ApiError> {
    let info = ClientInfo::from_headers(&headers);
    let ip = peer_ip(&state, &headers);

    // The body wins over the headers, because the body is the documented shape; the
    // headers are the fallback for a client that only announces itself once.
    let device_uid = request
        .device
        .as_ref()
        .map(|device| device.device_uid.clone())
        .or_else(|| request.device_uid.clone())
        .filter(|uid| !uid.trim().is_empty());

    let device = device_uid.map(|device_uid| ferroma_auth::DeviceInfo {
        device_uid,
        name: request
            .device
            .as_ref()
            .and_then(|device| device.name.clone()),
        platform: request
            .device
            .as_ref()
            .and_then(|device| device.platform.clone())
            .or_else(|| header_string(&headers, PLATFORM_HEADER)),
        client_version: request
            .device
            .as_ref()
            .and_then(|device| device.client_version.clone())
            .or_else(|| info.client_version.clone()),
        protocol_version: Some(info.protocol_or_default()),
    });

    let outcome = state
        .auth
        .login(
            &request.email,
            &request.password,
            SessionKind::Client,
            ip,
            info.user_agent.as_deref(),
            device,
        )
        .await
        .map_err(|err| match err {
            FerromaError::RateLimited => ApiError::new(FerromaError::RateLimited)
                .with_retry_after(state.config.limits.login_lockout_secs),
            other => ApiError::new(other),
        })?;

    let device_id = outcome
        .device
        .as_ref()
        .map(|device| device.id)
        .unwrap_or(0);

    let body = ClientLoginResponse {
        access_token: outcome.tokens.access_token,
        refresh_token: outcome.tokens.refresh_token,
        token_type: "Bearer".to_string(),
        expires_in: outcome.tokens.expires_in,
        device_id,
        user: UserResponse::from_row(&outcome.user),
    };

    let mut response = Json(body).into_response();
    negotiated_headers(&mut response, &state);
    Ok(response)
}

/// `POST /api/v1/client/auth/refresh`
pub async fn client_refresh(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<ClientRefreshRequest>,
) -> Result<Response, ApiError> {
    let ip = peer_ip(&state, &headers);
    let tokens = state
        .auth
        .refresh(&request.refresh_token, ip)
        .await
        .map_err(ApiError::from)?;

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

    // A rotated refresh token must keep belonging to the installation that asked for
    // it; a rotation that switched devices would be a silent takeover.
    if let Some(device_uid) = request.device_uid.as_deref().filter(|uid| !uid.trim().is_empty()) {
        if let Some(device_id) = session.device_id {
            let device = state
                .repos
                .devices
                .find_by_id(ferroma_core::DeviceId::new(device_id))
                .await?;
            match device {
                Some(device) if device.device_uid != device_uid => {
                    return Err(ApiError::new(FerromaError::Unauthorized(
                        "this refresh token belongs to a different device".to_string(),
                    )));
                }
                _ => {}
            }
        }
    }

    let user = state
        .repos
        .users
        .require_by_id(UserId::new(session.user_id))
        .await
        .map_err(ApiError::from)?;

    let body = ClientLoginResponse {
        access_token: tokens.access_token,
        refresh_token: tokens.refresh_token,
        token_type: "Bearer".to_string(),
        expires_in: tokens.expires_in,
        device_id: session.device_id.unwrap_or(0),
        user: UserResponse::from_row(&user),
    };

    let mut response = Json(body).into_response();
    negotiated_headers(&mut response, &state);
    Ok(response)
}

/// `POST /api/v1/client/auth/logout`
pub async fn client_logout(
    State(state): State<AppState>,
    client: ClientAuth,
    Json(_request): Json<ClientLogoutRequest>,
) -> Result<StatusCode, ApiError> {
    state
        .auth
        .logout(client.session_id())
        .await
        .map_err(ApiError::from)?;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /api/v1/client/account`
pub async fn client_account(
    State(state): State<AppState>,
    client: ClientAuth,
) -> Result<Response, ApiError> {
    let rows = state
        .repos
        .mailboxes
        .list_by_user(client.user_id())
        .await
        .map_err(ApiError::from)?;

    let mut mailboxes = Vec::with_capacity(rows.len());
    for row in rows {
        let domain = crate::routes::mail::store::domain_name(&state.repos, row.domain_id).await?;
        mailboxes.push(AddressBrief {
            id: row.id,
            address: row.address(&domain),
            is_primary: row.is_primary,
        });
    }

    let body = AccountResponse {
        user: AccountUser {
            id: client.user().id,
            email: client.user().email.clone(),
            display_name: client.user().display_name.clone(),
        },
        mailboxes,
        protocol_version: state.protocol_version(),
        min_protocol_version: state.min_protocol_version(),
        server_version: state.server_version().to_string(),
        server_hostname: state.config.server.hostname.clone(),
        limits: AccountLimits::from_config(&state.config),
        features: FEATURES.iter().map(|feature| feature.to_string()).collect(),
    };

    let mut response = Json(body).into_response();
    negotiated_headers(&mut response, &state);
    Ok(response)
}

/// Add the two headers `docs/fcp.md` §1 says the server answers with.
///
/// The protocol header always states the version the server *used*, which is the
/// server's own version even when the client asked for something higher — that is the
/// signal for the client to degrade gracefully.
pub fn negotiated_headers(response: &mut Response, state: &AppState) {
    if let Ok(value) = HeaderValue::from_str(&state.protocol_version().to_string()) {
        response
            .headers_mut()
            .insert(crate::extract::SERVER_PROTOCOL_HEADER, value);
    }
    if let Ok(value) = HeaderValue::from_str(state.server_version()) {
        response
            .headers_mut()
            .insert(crate::extract::SERVER_VERSION_HEADER, value);
    }
}

/// The header value a client should expect from its own `X-Ferroma-Protocol`.
pub fn negotiated_protocol(requested: Option<u32>, server: u32) -> u32 {
    match requested {
        // A client asking for more than this build speaks is served at the server's
        // version, and told so by the response header.
        Some(requested) if requested > server => server,
        Some(requested) => requested,
        None => crate::extract::ASSUMED_PROTOCOL_VERSION,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_account_limits_come_from_configuration() {
        let config = ferroma_core::Config::default();
        let limits = AccountLimits::from_config(&config);
        assert_eq!(limits.max_message_size, 26_214_400);
        assert_eq!(limits.max_recipients, 100);
        assert_eq!(limits.attachment_chunk_size, 1_048_576);
        assert_eq!(limits.sync_page_size, 500);
    }

    #[test]
    fn the_features_list_is_the_documented_one() {
        assert_eq!(
            FEATURES.to_vec(),
            vec!["sync", "events", "drafts", "attachments", "devices", "search"]
        );
    }

    #[test]
    fn negotiation_degrades_a_greedy_client_to_the_server_version() {
        assert_eq!(negotiated_protocol(Some(1), 1), 1);
        assert_eq!(negotiated_protocol(Some(2), 1), 1);
        assert_eq!(negotiated_protocol(Some(0), 1), 0);
        assert_eq!(negotiated_protocol(None, 1), 1);
    }

    #[test]
    fn the_account_body_matches_the_documented_shape() {
        let body = AccountResponse {
            user: AccountUser {
                id: 7,
                email: "alice@example.com".into(),
                display_name: Some("Alice".into()),
            },
            mailboxes: vec![AddressBrief {
                id: 3,
                address: "alice@example.com".into(),
                is_primary: true,
            }],
            protocol_version: 1,
            min_protocol_version: 1,
            server_version: "0.1.0".into(),
            server_hostname: "mail.example.com".into(),
            limits: AccountLimits {
                max_message_size: 26_214_400,
                max_recipients: 100,
                attachment_chunk_size: 1_048_576,
                sync_page_size: 500,
            },
            features: FEATURES.iter().map(|f| f.to_string()).collect(),
        };
        let json = serde_json::to_value(&body).expect("must serialise");
        assert_eq!(json["user"]["id"], 7);
        assert_eq!(json["mailboxes"][0]["address"], "alice@example.com");
        assert_eq!(json["protocol_version"], 1);
        assert_eq!(json["min_protocol_version"], 1);
        assert_eq!(json["server_hostname"], "mail.example.com");
        assert_eq!(json["limits"]["attachment_chunk_size"], 1_048_576);
        assert_eq!(json["features"][0], "sync");
    }

    #[test]
    fn the_login_body_accepts_the_documented_device_object() {
        let request: ClientLoginRequest = serde_json::from_value(serde_json::json!({
            "email": "alice@example.com",
            "password": "hunter2",
            "device": {
                "device_uid": "3f2c",
                "name": "Alice's laptop",
                "platform": "windows",
                "client_version": "0.7.0"
            }
        }))
        .expect("must deserialise");
        let device = request.device.expect("device must be present");
        assert_eq!(device.device_uid, "3f2c");
        assert_eq!(device.platform.as_deref(), Some("windows"));
    }

    #[test]
    fn a_login_without_a_device_object_is_still_accepted() {
        let request: ClientLoginRequest = serde_json::from_value(serde_json::json!({
            "email": "alice@example.com",
            "password": "hunter2"
        }))
        .expect("must deserialise");
        assert!(request.device.is_none());
        assert!(request.device_uid.is_none());
    }

    #[test]
    fn the_login_response_carries_the_device_id() {
        let body = ClientLoginResponse {
            access_token: "eyJ".into(),
            refresh_token: "rt_x".into(),
            token_type: "Bearer".into(),
            expires_in: 3600,
            device_id: 12,
            user: UserResponse {
                id: 7,
                email: "alice@example.com".into(),
                display_name: None,
                enabled: true,
                is_admin: false,
                quota_bytes: 0,
                used_bytes: 0,
                last_login_at: None,
                created_at: chrono::Utc::now(),
            },
        };
        let json = serde_json::to_value(&body).expect("must serialise");
        assert_eq!(json["device_id"], 12);
        assert_eq!(json["token_type"], "Bearer");
        assert_eq!(json["user"]["email"], "alice@example.com");
    }

    #[test]
    fn the_refresh_and_logout_bodies_are_optional_where_they_can_be() {
        let refresh: ClientRefreshRequest = serde_json::from_value(serde_json::json!({
            "refresh_token": "rt_x"
        }))
        .expect("must deserialise");
        assert!(refresh.device_uid.is_none());

        let logout: ClientLogoutRequest =
            serde_json::from_value(serde_json::json!({})).expect("must deserialise");
        assert!(logout.refresh_token.is_none());
    }

    #[test]
    fn a_refresh_without_a_token_is_refused() {
        assert!(serde_json::from_value::<ClientRefreshRequest>(serde_json::json!({})).is_err());
    }
}
