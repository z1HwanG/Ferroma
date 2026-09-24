//! Second factors and application passwords: `POST /api/v1/auth/totp/*` and
//! `/api/v1/auth/app-passwords`.
//!
//! Two rules shape this surface.
//!
//! **An enrollment is not enforced until a code proves the authenticator holds the
//! secret.** A mis-scanned QR code would otherwise lock the account out at the next
//! login, and the user would have no way back in.
//!
//! **Turning the factor off costs the account password**, not just a live session.
//! A stolen session is exactly the situation the factor protects against, so a
//! session alone must not be able to remove it.

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};

use ferroma_auth::TotpStatus;

use crate::error::ApiError;
use crate::extract::AuthUser;
use crate::state::AppState;

/// The `GET /api/v1/auth/totp` body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TotpStatusResponse {
    /// `disabled`, `pending` or `enabled`.
    ///
    /// `pending` is an enrollment whose secret was issued but never proved; it is
    /// **not** enforced, and the next enrollment replaces it.
    pub status: String,
    /// How many unused recovery codes are left.
    pub recovery_codes_left: i64,
}

/// The `POST /api/v1/auth/totp/enroll` body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnrollmentResponse {
    /// The base32 shared secret, shown once so the user can also type it by hand.
    pub secret: String,
    /// The `otpauth://` URI to render as a QR code.
    pub uri: String,
}

/// The `POST /api/v1/auth/totp/confirm` body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfirmRequest {
    /// The six-digit code the authenticator is showing.
    pub code: String,
}

/// What a completed enrollment returns.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfirmResponse {
    /// Whether the factor is now enforced.
    pub enabled: bool,
    /// The recovery codes, in plaintext, **exactly once**. Only digests are stored,
    /// so this response is the only chance to keep them.
    pub recovery_codes: Vec<String>,
}

/// The `POST /api/v1/auth/totp/disable` body.
#[derive(Debug, Clone, Deserialize)]
pub struct DisableRequest {
    /// The account password. See the module note: a live session is not enough.
    pub password: String,
}

/// The `POST /api/v1/auth/app-passwords` body.
#[derive(Debug, Clone, Deserialize)]
pub struct CreateAppPasswordRequest {
    /// What the password is for, so the list is readable later.
    pub label: String,
}

/// One application password, never its secret.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppPasswordResponse {
    /// The row id, used to revoke it.
    pub id: i64,
    /// The user-supplied label.
    pub label: String,
    /// When it was created.
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// When it was last accepted, or `null` if it never has been.
    pub last_used_at: Option<chrono::DateTime<chrono::Utc>>,
    /// When it was revoked, or `null` while it still works.
    pub revoked_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl From<ferroma_storage::repository::AppPassword> for AppPasswordResponse {
    fn from(row: ferroma_storage::repository::AppPassword) -> Self {
        AppPasswordResponse {
            id: row.id,
            label: row.label,
            created_at: row.created_at,
            last_used_at: row.last_used_at,
            revoked_at: row.revoked_at,
        }
    }
}

/// A fresh application password: the secret is here, once.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreatedAppPasswordResponse {
    /// The row, for the list the client already holds.
    #[serde(flatten)]
    pub password: AppPasswordResponse,
    /// The secret itself. It is not stored in plaintext and cannot be read again.
    pub secret: String,
}

/// The list body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppPasswordListResponse {
    /// Every password, revoked ones included.
    pub items: Vec<AppPasswordResponse>,
}

/// `GET /api/v1/auth/totp`
pub async fn totp_status(
    State(state): State<AppState>,
    user: AuthUser,
) -> Result<Json<TotpStatusResponse>, ApiError> {
    let status = state.auth.totp_status(user.user_id()).await?;
    let recovery_codes_left = state.auth.recovery_codes_left(user.user_id()).await?;
    Ok(Json(TotpStatusResponse {
        status: status_str(status).to_string(),
        recovery_codes_left,
    }))
}

/// `POST /api/v1/auth/totp/enroll`
///
/// Issuing a new secret while one is pending simply replaces it. Issuing one while
/// the factor is **enabled** is refused: the response would hand a fresh secret to
/// whoever holds the session, which is a way around the factor rather than a way to
/// set it up.
pub async fn totp_enroll(
    State(state): State<AppState>,
    user: AuthUser,
) -> Result<Json<EnrollmentResponse>, ApiError> {
    if state.auth.totp_enabled(user.user_id()).await? {
        return Err(ApiError::new(ferroma_core::FerromaError::Conflict(
            "second-factor authentication is already enabled; disable it first".to_string(),
        )));
    }
    let enrollment = state.auth.begin_totp_enrollment(user.user()).await?;
    Ok(Json(EnrollmentResponse {
        secret: enrollment.secret,
        uri: enrollment.uri,
    }))
}

/// `POST /api/v1/auth/totp/confirm`
pub async fn totp_confirm(
    State(state): State<AppState>,
    user: AuthUser,
    Json(request): Json<ConfirmRequest>,
) -> Result<Json<ConfirmResponse>, ApiError> {
    let recovery_codes = state
        .auth
        .confirm_totp_enrollment(user.user_id(), &request.code)
        .await?;
    Ok(Json(ConfirmResponse {
        enabled: true,
        recovery_codes,
    }))
}

/// `POST /api/v1/auth/totp/disable`
pub async fn totp_disable(
    State(state): State<AppState>,
    user: AuthUser,
    Json(request): Json<DisableRequest>,
) -> Result<Json<TotpStatusResponse>, ApiError> {
    // Re-verify the password: a stolen session must not be able to remove the
    // factor that exists to contain it.
    let matched = state
        .auth
        .verify_password(&request.password, &user.user().password_hash)
        .await;
    if !matched {
        return Err(ApiError::new(ferroma_core::FerromaError::Unauthorized(
            "the password did not match".to_string(),
        )));
    }
    state.auth.disable_totp(user.user_id()).await?;
    Ok(Json(TotpStatusResponse {
        status: status_str(TotpStatus::Disabled).to_string(),
        recovery_codes_left: 0,
    }))
}

/// `GET /api/v1/auth/app-passwords`
pub async fn list_app_passwords(
    State(state): State<AppState>,
    user: AuthUser,
) -> Result<Json<AppPasswordListResponse>, ApiError> {
    let items = state
        .auth
        .list_app_passwords(user.user_id())
        .await?
        .into_iter()
        .map(AppPasswordResponse::from)
        .collect();
    Ok(Json(AppPasswordListResponse { items }))
}

/// `POST /api/v1/auth/app-passwords`
pub async fn create_app_password(
    State(state): State<AppState>,
    user: AuthUser,
    Json(request): Json<CreateAppPasswordRequest>,
) -> Result<(StatusCode, Json<CreatedAppPasswordResponse>), ApiError> {
    let (row, secret) = state
        .auth
        .create_app_password(user.user_id(), &request.label)
        .await?;
    Ok((
        StatusCode::CREATED,
        Json(CreatedAppPasswordResponse {
            password: AppPasswordResponse::from(row),
            secret,
        }),
    ))
}

/// `DELETE /api/v1/auth/app-passwords/:id`
pub async fn revoke_app_password(
    State(state): State<AppState>,
    user: AuthUser,
    axum::extract::Path(id): axum::extract::Path<i64>,
) -> Result<StatusCode, ApiError> {
    let revoked = state.auth.revoke_app_password(user.user_id(), id).await?;
    if !revoked {
        return Err(ApiError::new(ferroma_core::FerromaError::NotFound(
            "no such application password".to_string(),
        )));
    }
    Ok(StatusCode::NO_CONTENT)
}

/// The wire name of an enrollment state.
fn status_str(status: TotpStatus) -> &'static str {
    match status {
        TotpStatus::Disabled => "disabled",
        TotpStatus::Pending => "pending",
        TotpStatus::Enabled => "enabled",
    }
}
