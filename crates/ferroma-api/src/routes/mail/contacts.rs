//! Contacts: addresses remembered from mail, plus the ones the owner adds.
//!
//! A blocked address is delivered to Junk. Deleting a contact forgets it; the next
//! message that names the address remembers it again, unblocked.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use ferroma_core::FerromaError;
use serde::{Deserialize, Serialize};

use crate::error::ApiError;
use crate::extract::AuthUser;
use crate::state::AppState;

/// One contact, as the console lists it.
#[derive(Debug, Serialize)]
pub struct ContactResponse {
    /// The row id.
    pub id: i64,
    /// The address, lower-case.
    pub address: String,
    /// A name, when one was given.
    pub display_name: Option<String>,
    /// A note, when one was given.
    pub note: Option<String>,
    /// Whether the owner marked it as a favorite.
    pub favorite: bool,
    /// Whether mail from it is delivered to Junk.
    pub blocked: bool,
}

impl From<ferroma_storage::models::Contact> for ContactResponse {
    fn from(row: ferroma_storage::models::Contact) -> Self {
        ContactResponse {
            id: row.id,
            address: row.address,
            display_name: row.display_name,
            note: row.note,
            favorite: row.favorite,
            blocked: row.blocked,
        }
    }
}

/// `GET /api/v1/contacts?q=`
#[derive(Debug, Deserialize)]
pub struct ContactQuery {
    /// Matches the address, the name or the note. Empty lists everything.
    #[serde(default)]
    pub q: String,
}

/// `POST /api/v1/contacts`
#[derive(Debug, Deserialize)]
pub struct CreateContactRequest {
    /// The address.
    pub address: String,
    /// A name.
    pub display_name: Option<String>,
}

/// `PATCH /api/v1/contacts/:id`
#[derive(Debug, Deserialize)]
pub struct UpdateContactRequest {
    /// A replacement name. An empty string clears it.
    pub display_name: Option<String>,
    /// A replacement note. An empty string clears it.
    pub note: Option<String>,
    /// Whether it is a favorite.
    pub favorite: Option<bool>,
    /// Whether mail from it goes to Junk.
    pub blocked: Option<bool>,
}

/// `GET /api/v1/contacts`
pub async fn list_contacts(
    State(state): State<AppState>,
    auth: AuthUser,
    Query(query): Query<ContactQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let rows = state
        .repos
        .contacts
        .list(auth.user_id(), &query.q, 200)
        .await?;
    let items: Vec<ContactResponse> = rows.into_iter().map(ContactResponse::from).collect();
    Ok(Json(serde_json::json!({ "items": items })))
}

/// `POST /api/v1/contacts`
pub async fn create_contact(
    State(state): State<AppState>,
    auth: AuthUser,
    Json(request): Json<CreateContactRequest>,
) -> Result<(StatusCode, Json<ContactResponse>), ApiError> {
    let address = request.address.trim();
    if ferroma_core::address::EmailAddress::parse(address).is_err() {
        return Err(ApiError::new(FerromaError::Invalid(format!(
            "{address} is not an address"
        ))));
    }
    let row = state
        .repos
        .contacts
        .remember(auth.user_id(), address, request.display_name.as_deref())
        .await?;
    Ok((StatusCode::CREATED, Json(ContactResponse::from(row))))
}

/// `PATCH /api/v1/contacts/:id`
pub async fn update_contact(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<i64>,
    Json(request): Json<UpdateContactRequest>,
) -> Result<Json<ContactResponse>, ApiError> {
    let row = state
        .repos
        .contacts
        .update(
            auth.user_id(),
            id,
            request.display_name.as_deref(),
            request.note.as_deref(),
            request.favorite,
            request.blocked,
        )
        .await?;
    Ok(Json(ContactResponse::from(row)))
}

/// `DELETE /api/v1/contacts/:id`
pub async fn delete_contact(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<i64>,
) -> Result<StatusCode, ApiError> {
    state.repos.contacts.delete(auth.user_id(), id).await?;
    Ok(StatusCode::NO_CONTENT)
}
