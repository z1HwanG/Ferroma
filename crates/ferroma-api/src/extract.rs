//! Request extractors: authentication, client identification and pagination.
//!
//! Two audiences authenticate differently (`docs/api.md` §1.2):
//!
//! | Surface | Mechanism |
//! |---|---|
//! | Management API | `Authorization: Bearer <token>` **or** a `ferroma_session` cookie |
//! | Client API (FCP) | bearer token only, plus the `X-Ferroma-*` version headers |
//!
//! [`AuthUser`] implements the management rule, [`AdminUser`] adds the admin bit and
//! [`ClientAuth`] implements the FCP rule together with the version gate from
//! `docs/api.md` §6. All three go through
//! [`ferroma_auth::AuthService::authenticate`], so a revoked session is rejected on
//! the very next request rather than when its access token happens to expire.
//!
//! [`ClientInfo`] is the audit-trail half: it parses the client's self-description
//! without demanding one, because `curl` and monitoring probes must still be able to
//! call the API.

use axum::extract::{FromRequestParts, Query};
use axum::http::request::Parts;
use ferroma_auth::{Authenticated, SessionKind};
use ferroma_core::{FerromaError, UserId};

use crate::error::{protocol_upgrade_required, ApiError};
use crate::state::AppState;

/// The cookie Webmail uses for its session.
pub const SESSION_COOKIE: &str = "ferroma_session";

/// The header naming the calling client, e.g. `FerromaClient/0.7.0`.
pub const CLIENT_HEADER: &str = "x-ferroma-client";
/// The header carrying the FCP protocol version the client speaks.
pub const PROTOCOL_HEADER: &str = "x-ferroma-protocol";
/// The header naming the client's platform.
pub const PLATFORM_HEADER: &str = "x-ferroma-platform";
/// The header the server answers with, naming the protocol version it used.
pub const SERVER_PROTOCOL_HEADER: &str = "x-ferroma-protocol";
/// The header the server answers with, naming its own version.
pub const SERVER_VERSION_HEADER: &str = "x-ferroma-server";

/// The protocol version assumed when a client sends no `X-Ferroma-Protocol`.
///
/// `docs/fcp.md` §1: a missing header is treated as protocol `1`, as a courtesy for
/// `curl` and monitoring.
pub const ASSUMED_PROTOCOL_VERSION: u32 = 1;

/// An authenticated request on the management surface.
#[derive(Debug, Clone)]
pub struct AuthUser(pub Authenticated);

impl AuthUser {
    /// The account behind the credential.
    pub fn authenticated(&self) -> &Authenticated {
        &self.0
    }

    /// The account.
    pub fn user(&self) -> &ferroma_storage::models::User {
        &self.0.user
    }

    /// The user's id.
    pub fn user_id(&self) -> UserId {
        self.0.user_id()
    }

    /// The session the credential belongs to.
    pub fn session(&self) -> &ferroma_storage::models::Session {
        &self.0.session
    }

    /// The session id.
    pub fn session_id(&self) -> ferroma_core::SessionId {
        self.0.session_id()
    }

    /// Whether the account may use the Admin API.
    pub fn is_admin(&self) -> bool {
        self.0.user.is_admin
    }

    /// The peer address recorded on the session, when there is one.
    pub fn ip(&self) -> Option<&str> {
        self.0.session.ip.as_deref()
    }

    /// The `User-Agent` recorded on the session, when there is one.
    pub fn user_agent(&self) -> Option<&str> {
        self.0.session.user_agent.as_deref()
    }
}

/// An authenticated request that additionally requires the admin bit.
///
/// An authenticated non-admin gets `403 forbidden`; an unauthenticated request gets
/// `401 unauthorized`, so the two are never confused.
#[derive(Debug, Clone)]
pub struct AdminUser(pub AuthUser);

impl AdminUser {
    /// The wrapped authentication.
    pub fn auth(&self) -> &AuthUser {
        &self.0
    }

    /// The account.
    pub fn user(&self) -> &ferroma_storage::models::User {
        self.0.user()
    }

    /// The user's id.
    pub fn user_id(&self) -> UserId {
        self.0.user_id()
    }
}

/// The caller's self-description, from the `X-Ferroma-*` headers (`docs/fcp.md` §1).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ClientInfo {
    /// `X-Ferroma-Client`, e.g. `FerromaClient/0.7.0`.
    pub client: Option<String>,
    /// The version parsed out of [`ClientInfo::client`], when it carried one.
    pub client_version: Option<String>,
    /// `X-Ferroma-Platform`, e.g. `windows`.
    pub platform: Option<String>,
    /// `X-Ferroma-Protocol` as sent. `None` when the header was absent.
    pub protocol: Option<u32>,
    /// `User-Agent`, for the audit trail.
    pub user_agent: Option<String>,
}

impl ClientInfo {
    /// Parse the client headers out of a request.
    pub fn from_headers(headers: &axum::http::HeaderMap) -> Self {
        let client = header_string(headers, CLIENT_HEADER);
        ClientInfo {
            client_version: client.as_deref().and_then(version_from_client_header),
            client,
            platform: header_string(headers, PLATFORM_HEADER),
            protocol: header_string(headers, PROTOCOL_HEADER).and_then(|raw| raw.trim().parse().ok()),
            user_agent: header_string(headers, "user-agent"),
        }
    }

    /// The protocol version to negotiate with: the header, or the documented default.
    pub fn protocol_or_default(&self) -> u32 {
        self.protocol.unwrap_or(ASSUMED_PROTOCOL_VERSION)
    }

    /// A one-line description for the audit trail.
    pub fn describe(&self) -> String {
        match (&self.client, &self.platform) {
            (Some(client), Some(platform)) => format!("{client} ({platform})"),
            (Some(client), None) => client.clone(),
            (None, Some(platform)) => format!("unknown client ({platform})"),
            (None, None) => "unknown client".to_string(),
        }
    }
}

impl<S> FromRequestParts<S> for ClientInfo
where
    S: Send + Sync,
{
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(ClientInfo::from_headers(&parts.headers))
    }
}

/// An authenticated request on the Client API (FCP) surface.
///
/// Unlike [`AuthUser`] this accepts **only** a bearer token — a browser cookie must
/// never be able to drive the desktop-client protocol — and it enforces the protocol
/// floor from `docs/api.md` §6.
#[derive(Debug, Clone)]
pub struct ClientAuth {
    /// The authenticated session.
    pub auth: Authenticated,
    /// The client's self-description.
    pub info: ClientInfo,
    /// The device the session belongs to, when it has one.
    pub device_id: Option<ferroma_core::DeviceId>,
}

impl ClientAuth {
    /// The account.
    pub fn user(&self) -> &ferroma_storage::models::User {
        &self.auth.user
    }

    /// The user's id.
    pub fn user_id(&self) -> UserId {
        self.auth.user_id()
    }

    /// The session id.
    pub fn session_id(&self) -> ferroma_core::SessionId {
        self.auth.session_id()
    }

    /// The device this request came from, when it came from a registered one.
    pub fn device(&self) -> Option<ferroma_core::DeviceId> {
        self.device_id
    }
}

/// Read one header as a trimmed `String`, ignoring an empty value.
pub fn header_string(headers: &axum::http::HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// Pull the version out of `FerromaClient/0.7.0`.
fn version_from_client_header(raw: &str) -> Option<String> {
    let (_, version) = raw.split_once('/')?;
    let version = version.split_whitespace().next().unwrap_or(version).trim();
    if version.is_empty() {
        None
    } else {
        Some(version.to_string())
    }
}

/// Extract a bearer token from the `Authorization` header.
///
/// The scheme is matched case-insensitively, as RFC 7235 requires, and a non-bearer
/// scheme yields `None` rather than an error so the cookie path can still be tried.
pub fn bearer_token(headers: &axum::http::HeaderMap) -> Option<String> {
    let raw = headers.get(axum::http::header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = raw.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let token = token.trim();
    if token.is_empty() {
        None
    } else {
        Some(token.to_string())
    }
}

/// Extract the `ferroma_session` cookie value.
pub fn session_cookie(headers: &axum::http::HeaderMap) -> Option<String> {
    let raw = headers.get(axum::http::header::COOKIE)?.to_str().ok()?;
    for pair in raw.split(';') {
        let pair = pair.trim();
        let Some((name, value)) = pair.split_once('=') else {
            continue;
        };
        if name.trim() == SESSION_COOKIE {
            let value = value.trim().trim_matches('"');
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

/// Resolve a credential into an [`Authenticated`], whatever surface produced it.
///
/// `cookie_allowed` is what separates the two surfaces: the management API takes the
/// cookie, the Client API does not.
pub async fn authenticate(
    state: &AppState,
    headers: &axum::http::HeaderMap,
    cookie_allowed: bool,
) -> Result<Authenticated, ApiError> {
    if let Some(token) = bearer_token(headers) {
        return state.auth.authenticate(&token).await.map_err(ApiError::from);
    }

    if cookie_allowed {
        if let Some(secret) = session_cookie(headers) {
            return authenticate_cookie(state, &secret).await;
        }
    }

    Err(ApiError::new(FerromaError::Unauthorized(
        "this endpoint requires an Authorization: Bearer token or a session cookie".to_string(),
    )))
}

/// Resolve the `ferroma_session` cookie.
///
/// `ferroma-auth` exposes [`ferroma_auth::AuthService::authenticate`] for bearer
/// tokens (which names the session it belongs to) and
/// [`ferroma_auth::AuthService::refresh`] for rotating refresh tokens, but a cookie
/// is neither: it is the raw value whose SHA-256 a `sessions` row stores. The lookup
/// therefore goes through the same public repositories the auth crate uses, applying
/// the same three checks — the row must exist, be unrevoked and unexpired, and its
/// owner must still be enabled — so a cookie can never outlive the session it names.
async fn authenticate_cookie(state: &AppState, secret: &str) -> Result<Authenticated, ApiError> {
    // The same function the auth crate writes with, so the two can never diverge.
    let hash = ferroma_auth::token::hash_token(secret);
    let session = state
        .repos
        .sessions
        .find_by_token_hash(&hash)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(|| {
            ApiError::new(FerromaError::Unauthorized(
                "the session cookie is not valid".to_string(),
            ))
        })?;

    if !session.is_valid_at(chrono::Utc::now()) {
        return Err(ApiError::new(FerromaError::Unauthorized(
            "session expired or revoked".to_string(),
        )));
    }

    let user_id = UserId::new(session.user_id);
    let user = state
        .repos
        .users
        .find_by_id(user_id)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(|| {
            ApiError::new(FerromaError::Unauthorized(
                "account no longer exists".to_string(),
            ))
        })?;
    if !user.enabled {
        return Err(ApiError::new(FerromaError::Unauthorized(
            "account disabled".to_string(),
        )));
    }

    // Best-effort: keep the session's last-seen activity honest for the Admin panel.
    if let Err(err) = state.repos.sessions.touch(session.session_id(), None).await {
        tracing::debug!(error = %err, "could not touch the session");
    }

    Ok(Authenticated {
        user,
        session,
        claims: None,
    })
}

impl FromRequestParts<AppState> for AuthUser {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        let auth = authenticate(state, &parts.headers, true).await?;
        Ok(AuthUser(auth))
    }
}

impl FromRequestParts<AppState> for AdminUser {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        let auth = authenticate(state, &parts.headers, true).await?;
        if !auth.user.is_admin {
            return Err(ApiError::new(FerromaError::Forbidden(
                "this endpoint requires an administrator".to_string(),
            )));
        }
        Ok(AdminUser(AuthUser(auth)))
    }
}

impl FromRequestParts<AppState> for ClientAuth {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        let info = ClientInfo::from_headers(&parts.headers);

        // Version negotiation comes first: a client too old to be understood must be
        // told so even when its credentials are also bad, because the upgrade is the
        // only thing it can act on.
        let minimum = state.min_protocol_version();
        let found = info.protocol_or_default();
        if found < minimum {
            return Err(protocol_upgrade_required(found, minimum));
        }

        let auth = authenticate(state, &parts.headers, false).await?;

        // A client session must come from the Client API; bearing a bearer token
        // minted for the browser surface is a bug worth naming.
        if let Some(kind) = SessionKind::parse(&auth.session.kind) {
            if !matches!(kind, SessionKind::Client | SessionKind::Api) {
                return Err(ApiError::new(FerromaError::Forbidden(
                    "this credential belongs to a different API surface".to_string(),
                )));
            }
        }

        let device_id = auth.session.device_id.map(ferroma_core::DeviceId::new);

        // A revoked device must be refused immediately, even though its access token
        // is still cryptographically valid: that is the whole point of revocation.
        if let Some(id) = device_id {
            let device = state
                .repos
                .devices
                .find_by_id(id)
                .await
                .map_err(ApiError::from)?;
            match device {
                Some(device) if device.revoked_at.is_some() => {
                    return Err(ApiError::new(FerromaError::Unauthorized(
                        "this device has been revoked".to_string(),
                    )));
                }
                _ => {}
            }
        }

        Ok(ClientAuth {
            auth,
            info,
            device_id,
        })
    }
}

/// The header a management-surface caller uses to make a mutation idempotent.
pub const IDEMPOTENCY_HEADER: &str = "idempotency-key";

/// The `Idempotency-Key` of a request, when it carried one.
///
/// `docs/api.md` §1.5: *"Any `POST` that changes state accepts an `Idempotency-Key`
/// header (Client API: `operation_id` in the body)."* The server records the operation
/// and replays the original response for a repeat, so a client that retries after a
/// timeout never double-sends or double-deletes.
///
/// An absent or blank header is `None` — the request simply is not recorded, which is
/// what a non-retrying caller wants.
#[derive(Debug, Clone)]
pub struct IdempotencyKey(pub Option<String>);

impl IdempotencyKey {
    /// The key, when there is a usable one.
    pub fn as_deref(&self) -> Option<&str> {
        self.0.as_deref()
    }

    /// Whether this request carries a key.
    pub fn is_present(&self) -> bool {
        self.0.is_some()
    }
}

impl<S> FromRequestParts<S> for IdempotencyKey
where
    S: Send + Sync,
{
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(IdempotencyKey(header_string(
            &parts.headers,
            IDEMPOTENCY_HEADER,
        )))
    }
}

/// A client-authenticated request that also requires the admin bit.
#[derive(Debug, Clone)]
pub struct ClientAdmin(pub ClientAuth);

impl FromRequestParts<AppState> for ClientAdmin {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        let client = ClientAuth::from_request_parts(parts, state).await?;
        if !client.auth.user.is_admin {
            return Err(ApiError::new(FerromaError::Forbidden(
                "this endpoint requires an administrator".to_string(),
            )));
        }
        Ok(ClientAdmin(client))
    }
}

/// The default page size for every list endpoint.
pub const DEFAULT_LIMIT: i64 = 50;
/// The largest page a client may ask for.
pub const MAX_LIMIT: i64 = 500;

/// `limit`/`offset` with the documented bounds (`docs/api.md` §1.4).
#[derive(Debug, Clone, Copy)]
pub struct Pagination {
    /// Page size, 1..=500, default 50.
    pub limit: i64,
    /// Rows to skip, `>= 0`.
    pub offset: i64,
}

/// The raw query pair, before clamping.
#[derive(Debug, Clone, Copy, Default, serde::Deserialize)]
pub struct PaginationQuery {
    /// Requested page size.
    pub limit: Option<i64>,
    /// Requested offset.
    pub offset: Option<i64>,
}

impl Pagination {
    /// Clamp a raw `limit`/`offset` pair into the documented window.
    ///
    /// Out-of-range values are clamped rather than rejected: `?limit=100000` is a
    /// client asking for "everything", and answering with the largest legal page is
    /// more useful than a `400`. A non-positive limit becomes the default, because a
    /// `0`-sized page would look like "no data" forever.
    pub fn clamped(limit: Option<i64>, offset: Option<i64>) -> Self {
        let limit = match limit {
            Some(value) if value > 0 => value.min(MAX_LIMIT),
            _ => DEFAULT_LIMIT,
        };
        Pagination {
            limit,
            offset: offset.unwrap_or(0).max(0),
        }
    }

    /// Wrap a page of items into the documented response shape.
    pub fn page<T>(&self, items: Vec<T>, total: i64) -> Page<T> {
        Page {
            items,
            total,
            limit: self.limit,
            offset: self.offset,
        }
    }
}

impl Default for Pagination {
    fn default() -> Self {
        Pagination {
            limit: DEFAULT_LIMIT,
            offset: 0,
        }
    }
}

impl<S> FromRequestParts<S> for Pagination
where
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, ApiError> {
        let Query(query) = Query::<PaginationQuery>::from_request_parts(parts, state)
            .await
            .map_err(|rejection| {
                // A malformed `limit=abc` is the caller's mistake, and the rejection's
                // own text names the offending value without revealing anything else.
                ApiError::new(FerromaError::Invalid(rejection.body_text()))
            })?;
        Ok(Pagination::clamped(query.limit, query.offset))
    }
}

/// The paginated response shape from `docs/api.md` §1.4.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Page<T> {
    /// The rows on this page.
    pub items: Vec<T>,
    /// How many rows the same filter matches in total.
    pub total: i64,
    /// The clamped page size.
    pub limit: i64,
    /// The offset these rows start at.
    pub offset: i64,
}

impl<T> Page<T> {
    /// Whether the page carries nothing.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// How many rows this page carries.
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Map every item, keeping the paging metadata.
    pub fn map<U>(self, mut f: impl FnMut(T) -> U) -> Page<U> {
        Page {
            items: self.items.into_iter().map(&mut f).collect(),
            total: self.total,
            limit: self.limit,
            offset: self.offset,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderMap, HeaderValue};

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(
                axum::http::HeaderName::from_bytes(name.as_bytes()).expect("valid header name"),
                HeaderValue::from_str(value).expect("valid header value"),
            );
        }
        map
    }

    #[test]
    fn bearer_tokens_parse_case_insensitively() {
        let map = headers(&[("authorization", "Bearer abc.def.ghi")]);
        assert_eq!(bearer_token(&map).as_deref(), Some("abc.def.ghi"));

        let map = headers(&[("authorization", "bearer   tok  ")]);
        assert_eq!(bearer_token(&map).as_deref(), Some("tok"));

        let map = headers(&[("authorization", "BASIC dXNlcjpwYXNz")]);
        assert_eq!(bearer_token(&map), None);

        let map = headers(&[("authorization", "Bearer")]);
        assert_eq!(bearer_token(&map), None);

        let map = headers(&[("authorization", "Bearer   ")]);
        assert_eq!(bearer_token(&map), None);

        assert_eq!(bearer_token(&HeaderMap::new()), None);
    }

    #[test]
    fn session_cookie_is_found_among_others() {
        let map = headers(&[("cookie", "theme=dark; ferroma_session=st_abc; locale=en")]);
        assert_eq!(session_cookie(&map).as_deref(), Some("st_abc"));

        let map = headers(&[("cookie", "ferroma_session=\"st_quoted\"")]);
        assert_eq!(session_cookie(&map).as_deref(), Some("st_quoted"));

        let map = headers(&[("cookie", "ferroma_session=; theme=dark")]);
        assert_eq!(session_cookie(&map), None);

        let map = headers(&[("cookie", "other=1")]);
        assert_eq!(session_cookie(&map), None);

        assert_eq!(session_cookie(&HeaderMap::new()), None);
    }

    #[test]
    fn client_info_parses_the_documented_headers() {
        let map = headers(&[
            ("x-ferroma-client", "FerromaClient/0.7.0"),
            ("x-ferroma-protocol", "1"),
            ("x-ferroma-platform", "windows"),
            ("user-agent", "FerromaClient/0.7.0 (Windows 11; x86_64)"),
        ]);
        let info = ClientInfo::from_headers(&map);
        assert_eq!(info.client.as_deref(), Some("FerromaClient/0.7.0"));
        assert_eq!(info.client_version.as_deref(), Some("0.7.0"));
        assert_eq!(info.platform.as_deref(), Some("windows"));
        assert_eq!(info.protocol, Some(1));
        assert_eq!(info.protocol_or_default(), 1);
        assert!(info.describe().contains("windows"));
    }

    #[test]
    fn a_missing_protocol_header_is_protocol_one() {
        let info = ClientInfo::from_headers(&HeaderMap::new());
        assert_eq!(info.protocol, None);
        assert_eq!(info.protocol_or_default(), ASSUMED_PROTOCOL_VERSION);
        assert_eq!(info.protocol_or_default(), 1);
        assert_eq!(info.describe(), "unknown client");
    }

    #[test]
    fn a_client_header_without_a_version_still_describes_itself() {
        let map = headers(&[("x-ferroma-client", "curl/8.4.0")]);
        let info = ClientInfo::from_headers(&map);
        assert_eq!(info.client.as_deref(), Some("curl/8.4.0"));
        assert_eq!(info.client_version.as_deref(), Some("8.4.0"));

        let map = headers(&[("x-ferroma-client", "somebody")]);
        let info = ClientInfo::from_headers(&map);
        assert_eq!(info.client_version, None);
    }

    #[test]
    fn a_garbage_protocol_header_is_treated_as_absent() {
        let map = headers(&[("x-ferroma-protocol", "not-a-number")]);
        let info = ClientInfo::from_headers(&map);
        assert_eq!(info.protocol, None);
        assert_eq!(info.protocol_or_default(), 1);
    }

    #[test]
    fn pagination_defaults_and_bounds() {
        let p = Pagination::clamped(None, None);
        assert_eq!(p.limit, DEFAULT_LIMIT);
        assert_eq!(p.offset, 0);

        // Below the floor and above the ceiling both clamp.
        assert_eq!(Pagination::clamped(Some(0), None).limit, DEFAULT_LIMIT);
        assert_eq!(Pagination::clamped(Some(-5), None).limit, DEFAULT_LIMIT);
        assert_eq!(Pagination::clamped(Some(10_000), None).limit, MAX_LIMIT);
        assert_eq!(Pagination::clamped(Some(MAX_LIMIT), None).limit, MAX_LIMIT);
        assert_eq!(Pagination::clamped(Some(1), None).limit, 1);

        // A negative offset is zero, not a database error.
        assert_eq!(Pagination::clamped(None, Some(-9)).offset, 0);
        assert_eq!(Pagination::clamped(None, Some(120)).offset, 120);

        assert_eq!(Pagination::default().limit, DEFAULT_LIMIT);
    }

    #[test]
    fn page_serialises_with_the_documented_field_names() {
        let page = Pagination::clamped(Some(2), Some(4)).page(vec!["a", "b"], 99);
        let json = serde_json::to_value(&page).expect("page must serialise");
        assert_eq!(json["items"], serde_json::json!(["a", "b"]));
        assert_eq!(json["total"], 99);
        assert_eq!(json["limit"], 2);
        assert_eq!(json["offset"], 4);
        assert_eq!(page.len(), 2);
        assert!(!page.is_empty());

        let empty: Page<u8> = Pagination::default().page(Vec::new(), 0);
        assert!(empty.is_empty());
        assert_eq!(empty.total, 0);
    }

    #[test]
    fn page_map_keeps_the_metadata() {
        let page = Pagination::clamped(Some(5), Some(10)).page(vec![1i64, 2], 7);
        let mapped = page.map(|value| value * 2);
        assert_eq!(mapped.items, vec![2, 4]);
        assert_eq!(mapped.total, 7);
        assert_eq!(mapped.limit, 5);
        assert_eq!(mapped.offset, 10);
    }

    #[test]
    fn header_string_ignores_blanks() {
        let map = headers(&[("x-ferroma-platform", "   ")]);
        assert_eq!(header_string(&map, PLATFORM_HEADER), None);
        let map = headers(&[("x-ferroma-platform", "linux")]);
        assert_eq!(header_string(&map, PLATFORM_HEADER).as_deref(), Some("linux"));
    }

    #[test]
    fn server_header_names_are_stable() {
        // The client asserts on these; changing one is a protocol break.
        assert_eq!(SERVER_PROTOCOL_HEADER, "x-ferroma-protocol");
        assert_eq!(SERVER_VERSION_HEADER, "x-ferroma-server");
        assert_eq!(SESSION_COOKIE, "ferroma_session");
        assert_eq!(IDEMPOTENCY_HEADER, "idempotency-key");
    }

    #[test]
    fn an_idempotency_key_is_read_only_when_it_is_usable() {
        let key = IdempotencyKey(header_string(
            &headers(&[(IDEMPOTENCY_HEADER, "  op_abc  ")]),
            IDEMPOTENCY_HEADER,
        ));
        assert_eq!(key.as_deref(), Some("op_abc"));
        assert!(key.is_present());

        let blank = IdempotencyKey(header_string(
            &headers(&[(IDEMPOTENCY_HEADER, "   ")]),
            IDEMPOTENCY_HEADER,
        ));
        assert_eq!(blank.as_deref(), None);
        assert!(!blank.is_present());

        let absent = IdempotencyKey(header_string(&HeaderMap::new(), IDEMPOTENCY_HEADER));
        assert!(absent.as_deref().is_none());
    }

    #[test]
    fn auth_user_accessors_delegate_to_the_authentication() {
        // Construct an `Authenticated` by hand: this test is about delegation, not
        // about a database round trip.
        let user = ferroma_storage::models::User {
            id: 7,
            email: "alice@example.com".into(),
            password_hash: String::new(),
            display_name: Some("Alice".into()),
            enabled: true,
            is_admin: true,
            quota_bytes: 100,
            used_bytes: 0,
            failed_logins: 0,
            locked_until: None,
            last_login_at: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        let session = ferroma_storage::models::Session {
            id: 3,
            user_id: 7,
            kind: "web".into(),
            token_hash: "hash".into(),
            device_id: Some(12),
            ip: Some("203.0.113.9".into()),
            user_agent: Some("Firefox".into()),
            created_at: chrono::Utc::now(),
            last_seen_at: chrono::Utc::now(),
            expires_at: chrono::Utc::now() + chrono::Duration::hours(1),
            revoked_at: None,
        };
        let auth = Authenticated {
            user,
            session,
            claims: None,
        };
        let user_view = AuthUser(auth);
        assert_eq!(user_view.user_id(), UserId::new(7));
        assert_eq!(user_view.session_id(), ferroma_core::SessionId::new(3));
        assert!(user_view.is_admin());
        assert_eq!(user_view.ip(), Some("203.0.113.9"));
        assert_eq!(user_view.user_agent(), Some("Firefox"));
        assert_eq!(user_view.user().email, "alice@example.com");

        let admin = AdminUser(user_view);
        assert!(admin.user().is_admin);
        assert_eq!(admin.user_id(), UserId::new(7));
    }

    #[test]
    fn cookie_hashing_matches_the_stored_form() {
        // The same function the auth crate hashes refresh tokens with, so a cookie
        // presented by Webmail is looked up the way it was stored.
        assert_eq!(
            ferroma_auth::token::hash_token("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_ne!(
            ferroma_auth::token::hash_token("st_abc"),
            ferroma_auth::token::hash_token("st_abd")
        );
    }
}
