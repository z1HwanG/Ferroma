//! Multi-account support (specification §31).
//!
//! An account is a row in SQLite, so the list survives a restart, and everything
//! else in the cache is scoped to it: two accounts — a personal one and a work
//! one — have separate folders, separate messages, separate outboxes and
//! separate cursors.
//!
//! Adding an account discovers the server (§32) unless the user typed one, logs
//! in, and stores the **refresh token in the account record** (`docs/fcp.md`
//! §2). Pausing an account stops its syncing without touching the other ones.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use ferroma_core::{EmailAddress, OperationId};
use sqlx::Row;

use crate::api::{DeviceInfo, FcpClient, RetryPolicy, TokenStore, UserRef};
use crate::autodiscover::{Autodiscover, Discovery, DiscoverySource};
use crate::database::ClientDatabase;
use crate::error::{BoxFuture, ClientError, ClientResult};
use crate::util::{now_rfc3339, parse_rfc3339};

/// The `settings` key holding this installation's device uid.
pub const DEVICE_UID_KEY: &str = "client.device_uid";

/// A row of `accounts`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AccountId(pub i64);

impl AccountId {
    /// Wrap a raw id.
    pub const fn new(raw: i64) -> Self {
        AccountId(raw)
    }

    /// The raw id.
    pub const fn get(self) -> i64 {
        self.0
    }
}

impl std::fmt::Display for AccountId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<i64> for AccountId {
    fn from(raw: i64) -> Self {
        AccountId(raw)
    }
}

/// A configured account.
#[derive(Debug, Clone, PartialEq)]
pub struct Account {
    /// The local row id.
    pub id: AccountId,
    /// The address the user signed in with.
    pub email: String,
    /// A friendly name shown in the sidebar.
    pub display_name: Option<String>,
    /// The FCP base URL, e.g. `https://mail.example.com/api/v1/client`.
    pub base_url: String,
    /// This installation's device uid.
    pub device_uid: String,
    /// The server's `devices` row for this installation.
    pub device_id: Option<i64>,
    /// The server's user id.
    pub user_id: Option<i64>,
    /// Whether sync is paused for this account (§31).
    pub paused: bool,
    /// `None` = sync everything, otherwise the §52 window.
    pub sync_window_days: Option<u32>,
    /// The §52 maximum cache size for this account.
    pub cache_limit_bytes: Option<u64>,
    /// The protocol version the server negotiated.
    pub protocol_version: Option<u32>,
    /// The server build.
    pub server_version: Option<String>,
    /// When the account was added.
    pub created_at: DateTime<Utc>,
    /// When it was last modified.
    pub updated_at: DateTime<Utc>,
    /// The last successful sync.
    pub last_sync_at: Option<DateTime<Utc>>,
    /// The last failure, for the account list.
    pub last_error: Option<String>,
    /// Whether a refresh token is stored, i.e. whether the account can sync
    /// unattended. The token itself is never exposed.
    pub refresh_token_present: bool,
}

impl Account {
    /// The label the sidebar shows.
    pub fn label(&self) -> String {
        match &self.display_name {
            Some(name) if !name.trim().is_empty() => name.clone(),
            _ => self.email.clone(),
        }
    }

    /// Whether the account carries a refresh token (i.e. can run unattended).
    pub fn has_credentials(&self) -> bool {
        self.refresh_token_present
    }

    /// Whether this account should be synced right now.
    pub fn is_syncable(&self) -> bool {
        !self.paused && self.has_credentials()
    }

    fn from_row(row: &sqlx::sqlite::SqliteRow) -> ClientResult<Self> {
        Ok(Account {
            id: AccountId(row.try_get("id")?),
            email: row.try_get("email")?,
            display_name: row.try_get("display_name")?,
            base_url: row.try_get("base_url")?,
            device_uid: row.try_get("device_uid")?,
            device_id: row.try_get("device_id")?,
            user_id: row.try_get("user_id")?,
            paused: row.try_get::<i64, _>("paused")? != 0,
            sync_window_days: row
                .try_get::<Option<i64>, _>("sync_window_days")?
                .map(|days| days.max(0) as u32),
            cache_limit_bytes: row
                .try_get::<Option<i64>, _>("cache_limit_bytes")?
                .map(|bytes| bytes.max(0) as u64),
            protocol_version: row
                .try_get::<Option<i64>, _>("protocol_version")?
                .map(|version| version.max(0) as u32),
            server_version: row.try_get("server_version")?,
            created_at: parse_rfc3339(&row.try_get::<String, _>("created_at")?)
                .unwrap_or_else(Utc::now),
            updated_at: parse_rfc3339(&row.try_get::<String, _>("updated_at")?)
                .unwrap_or_else(Utc::now),
            last_sync_at: row
                .try_get::<Option<String>, _>("last_sync_at")?
                .and_then(|raw| parse_rfc3339(&raw)),
            last_error: row.try_get("last_error")?,
            refresh_token_present: row
                .try_get::<Option<String>, _>("refresh_token")?
                .is_some(),
        })
    }
}

/// The sync state of one folder stream, for the account status view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamStatus {
    /// The folder, or `0` for the account-level stream.
    pub folder_id: i64,
    /// The stored cursor (opaque).
    pub cursor: String,
    /// `idle`, `syncing`, `error` or `paused`.
    pub status: String,
    /// The last failure on this stream.
    pub last_error: Option<String>,
    /// When the stream last advanced.
    pub last_sync_at: Option<DateTime<Utc>>,
}

/// The per-account sync status `§31` asks for.
#[derive(Debug, Clone, PartialEq)]
pub struct SyncStatus {
    /// The account.
    pub account_id: AccountId,
    /// Whether syncing is paused.
    pub paused: bool,
    /// The last successful sync.
    pub last_sync_at: Option<DateTime<Utc>>,
    /// The last failure.
    pub last_error: Option<String>,
    /// How many offline operations are waiting.
    pub pending_operations: i64,
    /// How many drafts still need pushing.
    pub dirty_drafts: i64,
    /// One entry per stream.
    pub streams: Vec<StreamStatus>,
}

impl SyncStatus {
    /// A one-line summary for the account list.
    pub fn summary(&self) -> String {
        if self.paused {
            return "paused".to_string();
        }
        match (&self.last_error, self.pending_operations) {
            (Some(err), _) => format!("error: {err}"),
            (None, 0) => "up to date".to_string(),
            (None, n) => format!("{n} operation(s) pending"),
        }
    }
}

/// The [`TokenStore`] that keeps the rotating refresh token in the account row
/// (`docs/fcp.md` §2).
#[derive(Debug, Clone)]
pub struct SqliteTokenStore {
    db: Arc<ClientDatabase>,
    account_id: AccountId,
}

impl SqliteTokenStore {
    /// Bind a store to an account row.
    pub fn new(db: Arc<ClientDatabase>, account_id: AccountId) -> Self {
        SqliteTokenStore { db, account_id }
    }
}

impl TokenStore for SqliteTokenStore {
    fn load(&self) -> BoxFuture<'_, Option<String>> {
        Box::pin(async move {
            let row = sqlx::query("SELECT refresh_token FROM accounts WHERE id = ?")
                .bind(self.account_id.get())
                .fetch_optional(self.db.pool())
                .await
                .ok()
                .flatten()?;
            row.try_get::<Option<String>, _>("refresh_token").ok().flatten()
        })
    }

    fn save(&self, refresh_token: String) -> BoxFuture<'_, ClientResult<()>> {
        Box::pin(async move {
            sqlx::query("UPDATE accounts SET refresh_token = ?, updated_at = ? WHERE id = ?")
                .bind(refresh_token)
                .bind(now_rfc3339())
                .bind(self.account_id.get())
                .execute(self.db.pool())
                .await?;
            Ok(())
        })
    }

    fn clear(&self) -> BoxFuture<'_, ClientResult<()>> {
        Box::pin(async move {
            sqlx::query("UPDATE accounts SET refresh_token = NULL, updated_at = ? WHERE id = ?")
                .bind(now_rfc3339())
                .bind(self.account_id.get())
                .execute(self.db.pool())
                .await?;
            Ok(())
        })
    }
}

/// Adds, removes, pauses and re-authenticates accounts (§31).
#[derive(Debug, Clone)]
pub struct AccountManager {
    db: Arc<ClientDatabase>,
    autodiscover: Autodiscover,
    retry: RetryPolicy,
}

impl AccountManager {
    /// Build a manager over a cache.
    pub fn new(db: Arc<ClientDatabase>) -> ClientResult<Self> {
        Ok(AccountManager {
            db,
            autodiscover: Autodiscover::new()?,
            retry: RetryPolicy::default(),
        })
    }

    /// Build a manager with an explicit retry policy (the CLI's `--no-retry`).
    pub fn with_retry(db: Arc<ClientDatabase>, retry: RetryPolicy) -> ClientResult<Self> {
        Ok(AccountManager {
            db,
            autodiscover: Autodiscover::new()?,
            retry,
        })
    }

    /// The cache behind the manager.
    pub fn database(&self) -> &Arc<ClientDatabase> {
        &self.db
    }

    /// The autodiscovery helper.
    pub fn autodiscover(&self) -> &Autodiscover {
        &self.autodiscover
    }

    /// This installation's device uid, generated once and remembered.
    pub async fn device_uid(&self) -> ClientResult<String> {
        if let Some(existing) = self.db.get_setting(DEVICE_UID_KEY).await? {
            if !existing.trim().is_empty() {
                return Ok(existing);
            }
        }
        let generated = format!("dev_{}", uuid::Uuid::new_v4().simple());
        self.db.put_setting(DEVICE_UID_KEY, &generated).await?;
        Ok(generated)
    }

    /// Every account, in the order they were added.
    pub async fn list(&self) -> ClientResult<Vec<Account>> {
        let rows = sqlx::query("SELECT * FROM accounts ORDER BY id ASC")
            .fetch_all(self.db.pool())
            .await?;
        rows.iter().map(Account::from_row).collect()
    }

    /// One account.
    pub async fn get(&self, id: AccountId) -> ClientResult<Option<Account>> {
        let row = sqlx::query("SELECT * FROM accounts WHERE id = ?")
            .bind(id.get())
            .fetch_optional(self.db.pool())
            .await?;
        match row {
            Some(row) => Ok(Some(Account::from_row(&row)?)),
            None => Ok(None),
        }
    }

    /// The first account whose address matches, case-insensitively.
    pub async fn find_by_email(&self, email: &str) -> ClientResult<Option<Account>> {
        let rows = sqlx::query("SELECT * FROM accounts ORDER BY id ASC")
            .fetch_all(self.db.pool())
            .await?;
        let wanted = email.trim().to_ascii_lowercase();
        for row in rows {
            let account = Account::from_row(&row)?;
            if account.email.to_ascii_lowercase() == wanted {
                return Ok(Some(account));
            }
        }
        Ok(None)
    }

    /// An [`FcpClient`] bound to an account, reading and writing that account's
    /// refresh token.
    pub async fn client(&self, id: AccountId) -> ClientResult<FcpClient> {
        self.client_with_retry(id, self.retry).await
    }

    /// The same, with an explicit retry policy (the CLI's `--no-retry`).
    pub async fn client_with_retry(
        &self,
        id: AccountId,
        retry: RetryPolicy,
    ) -> ClientResult<FcpClient> {
        let account = self
            .get(id)
            .await?
            .ok_or_else(|| ClientError::not_found(format!("no account {id}")))?;
        self.client_for(&account, retry)
    }

    fn client_for(&self, account: &Account, retry: RetryPolicy) -> ClientResult<FcpClient> {
        let store = Arc::new(SqliteTokenStore::new(self.db.clone(), account.id));
        FcpClient::with_retry(account.base_url.clone(), store, retry)
    }

    /// Add an account, discovering the server when the user did not name one.
    pub async fn add(
        &self,
        email: &str,
        password: &str,
        display_name: Option<String>,
        base_url: Option<String>,
    ) -> ClientResult<Account> {
        let address = EmailAddress::parse(email)
            .map_err(|err| ClientError::invalid(format!("{email:?} is not an address: {err}")))?;
        let account = match base_url {
            Some(base_url) => {
                self.insert(
                    &address.to_lowercase(),
                    display_name,
                    &base_url,
                    DiscoverySource::Guessed,
                )
                .await?
            }
            None => {
                let discovery = self.autodiscover.discover(address.domain()).await?;
                self.insert(
                    &address.to_lowercase(),
                    display_name,
                    &discovery.fcp_base_url(),
                    discovery.source,
                )
                .await?
            }
        };
        self.authenticate(&account, password).await
    }

    /// Add an account whose discovery result is already known.
    pub async fn add_discovered(
        &self,
        email: &str,
        password: &str,
        discovery: &Discovery,
        display_name: Option<String>,
    ) -> ClientResult<Account> {
        let address = EmailAddress::parse(email)
            .map_err(|err| ClientError::invalid(format!("{email:?} is not an address: {err}")))?;
        let account = self
            .insert(
                &address.to_lowercase(),
                display_name,
                &discovery.fcp_base_url(),
                discovery.source,
            )
            .await?;
        self.authenticate(&account, password).await
    }

    async fn insert(
        &self,
        email: &str,
        display_name: Option<String>,
        base_url: &str,
        source: DiscoverySource,
    ) -> ClientResult<Account> {
        let device_uid = self.device_uid().await?;
        let now = now_rfc3339();
        tracing::info!(email, base_url, discovery = ?source, "adding an account");
        let result = sqlx::query(
            "INSERT INTO accounts (email, display_name, base_url, device_uid, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(email)
        .bind(display_name)
        .bind(base_url)
        .bind(&device_uid)
        .bind(&now)
        .bind(&now)
        .execute(self.db.pool())
        .await?;
        self.get(AccountId(result.last_insert_rowid()))
            .await?
            .ok_or_else(|| ClientError::cache("the account row vanished after the insert"))
    }

    /// Log in and record what the server told us.
    async fn authenticate(&self, account: &Account, password: &str) -> ClientResult<Account> {
        let client = self.client_for(account, self.retry)?;
        let device = DeviceInfo::local(account.device_uid.clone(), account.label());
        let login = client.login(&account.email, password, &device).await?;
        let user: UserRef = login.user.clone();

        sqlx::query(
            "UPDATE accounts SET device_id = ?, user_id = ?, updated_at = ?, last_error = NULL
             WHERE id = ?",
        )
        .bind(login.device_id)
        .bind(user.id)
        .bind(now_rfc3339())
        .bind(account.id.get())
        .execute(self.db.pool())
        .await?;

        // The negotiated versions are nice to have; a failure here must not fail
        // the login.
        match client.account().await {
            Ok(info) => {
                sqlx::query(
                    "UPDATE accounts SET protocol_version = ?, server_version = ? WHERE id = ?",
                )
                .bind(info.protocol_version as i64)
                .bind(&info.server_version)
                .bind(account.id.get())
                .execute(self.db.pool())
                .await?;
            }
            Err(err) => {
                tracing::debug!(error = %err.user_message(), "could not read GET /account after login");
            }
        }

        self.get(account.id)
            .await?
            .ok_or_else(|| ClientError::not_found(format!("no account {}", account.id)))
    }

    /// Sign in again after a [`ClientError::SessionExpired`].
    pub async fn reauthenticate(&self, id: AccountId, password: &str) -> ClientResult<Account> {
        let account = self
            .get(id)
            .await?
            .ok_or_else(|| ClientError::not_found(format!("no account {id}")))?;
        self.authenticate(&account, password).await
    }

    /// Pause or resume syncing for one account. The other accounts are
    /// untouched.
    pub async fn pause(&self, id: AccountId, paused: bool) -> ClientResult<Account> {
        let affected = sqlx::query("UPDATE accounts SET paused = ?, updated_at = ? WHERE id = ?")
            .bind(i64::from(paused))
            .bind(now_rfc3339())
            .bind(id.get())
            .execute(self.db.pool())
            .await?
            .rows_affected();
        if affected == 0 {
            return Err(ClientError::not_found(format!("no account {id}")));
        }
        // The streams follow the account, so a later resume knows what to do.
        sqlx::query("UPDATE sync_state SET status = ? WHERE account_id = ?")
            .bind(if paused { "paused" } else { "idle" })
            .bind(id.get())
            .execute(self.db.pool())
            .await?;
        self.get(id)
            .await?
            .ok_or_else(|| ClientError::not_found(format!("no account {id}")))
    }

    /// Edit the user-facing settings of an account.
    pub async fn edit(
        &self,
        id: AccountId,
        display_name: Option<Option<String>>,
        sync_window_days: Option<Option<u32>>,
        cache_limit_bytes: Option<Option<u64>>,
    ) -> ClientResult<Account> {
        let account = self
            .get(id)
            .await?
            .ok_or_else(|| ClientError::not_found(format!("no account {id}")))?;
        let name = display_name.unwrap_or(account.display_name);
        let window = sync_window_days.unwrap_or(account.sync_window_days);
        let limit = cache_limit_bytes.unwrap_or(account.cache_limit_bytes);
        sqlx::query(
            "UPDATE accounts SET display_name = ?, sync_window_days = ?, cache_limit_bytes = ?,
             updated_at = ? WHERE id = ?",
        )
        .bind(&name)
        .bind(window.map(|days| days as i64))
        .bind(limit.map(|bytes| bytes as i64))
        .bind(now_rfc3339())
        .bind(id.get())
        .execute(self.db.pool())
        .await?;
        self.get(id)
            .await?
            .ok_or_else(|| ClientError::not_found(format!("no account {id}")))
    }

    /// Remove an account **and its whole cache** (§31).
    pub async fn remove(&self, id: AccountId) -> ClientResult<()> {
        if self.get(id).await?.is_none() {
            return Err(ClientError::not_found(format!("no account {id}")));
        }
        self.db.wipe_account(id.get()).await?;
        Ok(())
    }

    /// Record the outcome of a sync run.
    pub async fn record_sync(
        &self,
        id: AccountId,
        error: Option<&str>,
    ) -> ClientResult<()> {
        match error {
            Some(error) => {
                sqlx::query("UPDATE accounts SET last_error = ?, updated_at = ? WHERE id = ?")
                    .bind(error)
                    .bind(now_rfc3339())
                    .bind(id.get())
                    .execute(self.db.pool())
                    .await?;
            }
            None => {
                sqlx::query(
                    "UPDATE accounts SET last_sync_at = ?, last_error = NULL, updated_at = ? WHERE id = ?",
                )
                .bind(now_rfc3339())
                .bind(now_rfc3339())
                .bind(id.get())
                .execute(self.db.pool())
                .await?;
            }
        }
        Ok(())
    }

    /// The §31 status view.
    pub async fn status(&self, id: AccountId) -> ClientResult<SyncStatus> {
        let account = self
            .get(id)
            .await?
            .ok_or_else(|| ClientError::not_found(format!("no account {id}")))?;

        let streams = sqlx::query(
            "SELECT folder_id, cursor, status, last_error, last_sync_at FROM sync_state
             WHERE account_id = ? ORDER BY folder_id ASC",
        )
        .bind(id.get())
        .fetch_all(self.db.pool())
        .await?
        .iter()
        .map(|row| {
            Ok(StreamStatus {
                folder_id: row.try_get("folder_id")?,
                cursor: row.try_get("cursor")?,
                status: row.try_get("status")?,
                last_error: row.try_get("last_error")?,
                last_sync_at: row
                    .try_get::<Option<String>, _>("last_sync_at")?
                    .and_then(|raw| parse_rfc3339(&raw)),
            })
        })
        .collect::<ClientResult<Vec<_>>>()?;

        let pending: i64 =
            sqlx::query("SELECT COUNT(*) AS n FROM pending_operations WHERE account_id = ?")
                .bind(id.get())
                .fetch_one(self.db.pool())
                .await?
                .get("n");
        let dirty: i64 =
            sqlx::query("SELECT COUNT(*) AS n FROM drafts WHERE account_id = ? AND dirty = 1 AND deleted = 0")
                .bind(id.get())
                .fetch_one(self.db.pool())
                .await?
                .get("n");

        Ok(SyncStatus {
            account_id: id,
            paused: account.paused,
            last_sync_at: account.last_sync_at,
            last_error: account.last_error,
            pending_operations: pending,
            dirty_drafts: dirty,
            streams,
        })
    }

    /// Every account that should be synced right now.
    pub async fn syncable(&self) -> ClientResult<Vec<Account>> {
        Ok(self
            .list()
            .await?
            .into_iter()
            .filter(Account::is_syncable)
            .collect())
    }

    /// Queue an operation for an account without talking to the network.
    pub async fn queue_operation(
        &self,
        id: AccountId,
        kind: crate::operations::OperationKind,
        payload: serde_json::Value,
    ) -> ClientResult<OperationId> {
        if self.get(id).await?.is_none() {
            return Err(ClientError::not_found(format!("no account {id}")));
        }
        let queue = crate::operations::PendingQueue::new(self.db.clone());
        Ok(queue
            .enqueue(id.get(), kind, payload)
            .await?
            .operation_key())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{MockServer, TempDir};

    async fn manager() -> (TempDir, Arc<ClientDatabase>, AccountManager) {
        let dir = TempDir::new().expect("temp dir");
        let db = Arc::new(
            ClientDatabase::open(dir.path().join("cache.db"))
                .await
                .expect("open"),
        );
        let manager = AccountManager::new(db.clone()).expect("manager");
        (dir, db, manager)
    }

    async fn server_with_login() -> MockServer {
        let server = MockServer::start().await;
        server.route("POST", "/api/v1/client/auth/login", |req, _n| {
            let body = req.json();
            let email = body["email"].as_str().unwrap_or("unknown").to_string();
            crate::testutil::MockResponse::json(format!(
                r#"{{"access_token":"access","refresh_token":"rt_{email}","token_type":"Bearer",
                     "expires_in":3600,"device_id":12,"user":{{"id":7,"email":"{email}"}}}}"#
            ))
        });
        server.json_route(
            "GET",
            "/api/v1/client/account",
            200,
            r#"{"user":{"id":7,"email":"a@b.c"},"protocol_version":1,"server_version":"0.1.0"}"#,
        );
        server
    }

    fn discovery_for(server: &MockServer) -> Discovery {
        let mut discovery = Discovery::guessed("example.com");
        discovery.api = format!("{}/api/v1", server.base_url());
        discovery.source = DiscoverySource::Guessed;
        discovery
    }

    #[tokio::test]
    async fn adding_two_accounts_keeps_them_distinct() {
        let (_dir, _db, manager) = manager().await;
        let server = server_with_login().await;
        let discovery = discovery_for(&server);

        let first = manager
            .add_discovered("alice@example.com", "pw", &discovery, Some("Personal".into()))
            .await
            .expect("first");
        let second = manager
            .add_discovered("alice@company.com", "pw", &discovery, Some("Work".into()))
            .await
            .expect("second");

        assert_ne!(first.id, second.id);
        assert_eq!(first.label(), "Personal");
        assert_eq!(second.label(), "Work");
        let listed = manager.list().await.expect("list");
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].email, "alice@example.com");
        assert!(manager.find_by_email("ALICE@COMPANY.COM").await.expect("find").is_some());
    }

    #[tokio::test]
    async fn an_account_remembers_its_refresh_token() {
        let (_dir, _db, manager) = manager().await;
        let server = server_with_login().await;
        let discovery = discovery_for(&server);
        let account = manager
            .add_discovered("alice@example.com", "pw", &discovery, None)
            .await
            .expect("add");
        let store = SqliteTokenStore::new(manager.database().clone(), account.id);
        assert_eq!(store.load().await.as_deref(), Some("rt_alice@example.com"));

        let reloaded = manager.get(account.id).await.expect("get").expect("row");
        assert!(reloaded.has_credentials());
        assert!(reloaded.is_syncable());
        assert_eq!(reloaded.device_id, Some(12));
        assert_eq!(reloaded.user_id, Some(7));
        assert_eq!(reloaded.server_version.as_deref(), Some("0.1.0"));
        assert_eq!(reloaded.protocol_version, Some(1));
    }

    #[tokio::test]
    async fn pausing_one_account_does_not_pause_the_other() {
        let (_dir, _db, manager) = manager().await;
        let server = server_with_login().await;
        let discovery = discovery_for(&server);
        let first = manager
            .add_discovered("a@example.com", "pw", &discovery, None)
            .await
            .expect("first");
        let second = manager
            .add_discovered("b@example.com", "pw", &discovery, None)
            .await
            .expect("second");

        manager.pause(first.id, true).await.expect("pause");
        let paused = manager.get(first.id).await.expect("get").expect("row");
        let running = manager.get(second.id).await.expect("get").expect("row");
        assert!(paused.paused);
        assert!(!paused.is_syncable());
        assert!(!running.paused);
        assert!(running.is_syncable());

        let syncable = manager.syncable().await.expect("syncable");
        assert_eq!(syncable.len(), 1);
        assert_eq!(syncable[0].id, second.id);

        manager.pause(first.id, false).await.expect("resume");
        assert!(manager
            .get(first.id)
            .await
            .expect("get")
            .expect("row")
            .is_syncable());
    }

    #[tokio::test]
    async fn pausing_a_missing_account_is_not_found() {
        let (_dir, _db, manager) = manager().await;
        let err = manager.pause(AccountId(99), true).await.expect_err("must fail");
        assert!(matches!(err, ClientError::NotFound(_)));
    }

    #[tokio::test]
    async fn removing_an_account_removes_its_cache_and_leaves_the_others() {
        let (_dir, db, manager) = manager().await;
        let server = server_with_login().await;
        let discovery = discovery_for(&server);
        let first = manager
            .add_discovered("a@example.com", "pw", &discovery, None)
            .await
            .expect("first");
        let second = manager
            .add_discovered("b@example.com", "pw", &discovery, None)
            .await
            .expect("second");

        // Give both accounts some cached rows.
        for account in [first.id, second.id] {
            sqlx::query(
                "INSERT INTO folders (account_id, id, mailbox_id, name) VALUES (?, 5, 3, 'INBOX')",
            )
            .bind(account.get())
            .execute(db.pool())
            .await
            .expect("folder");
            sqlx::query(
                "INSERT INTO messages (account_id, id, folder_id, uid, cached_at) VALUES (?, 1, 5, 1, 'now')",
            )
            .bind(account.get())
            .execute(db.pool())
            .await
            .expect("message");
        }

        manager.remove(first.id).await.expect("remove");
        assert_eq!(db.count_account_rows("folders", first.id.get()).await.expect("count"), 0);
        assert_eq!(db.count_account_rows("messages", first.id.get()).await.expect("count"), 0);
        assert_eq!(db.count_account_rows("folders", second.id.get()).await.expect("count"), 1);
        assert_eq!(manager.list().await.expect("list").len(), 1);
        assert!(manager.remove(first.id).await.is_err(), "removing twice fails");
    }

    #[tokio::test]
    async fn the_account_list_survives_a_restart() {
        let dir = TempDir::new().expect("temp dir");
        let path = dir.path().join("cache.db");
        let server = server_with_login().await;
        let discovery = discovery_for(&server);
        let id = {
            let db = Arc::new(ClientDatabase::open(&path).await.expect("open"));
            let manager = AccountManager::new(db.clone()).expect("manager");
            let account = manager
                .add_discovered("a@example.com", "pw", &discovery, None)
                .await
                .expect("add");
            db.close().await;
            account.id
        };
        let db = Arc::new(ClientDatabase::open(&path).await.expect("reopen"));
        let manager = AccountManager::new(db).expect("manager");
        let listed = manager.list().await.expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, id);
        assert_eq!(listed[0].email, "a@example.com");
    }

    #[tokio::test]
    async fn reauthenticating_replaces_the_stored_token() {
        let (_dir, _db, manager) = manager().await;
        let server = server_with_login().await;
        let discovery = discovery_for(&server);
        let account = manager
            .add_discovered("a@example.com", "pw", &discovery, None)
            .await
            .expect("add");
        let store = SqliteTokenStore::new(manager.database().clone(), account.id);
        store.clear().await.expect("clear");
        assert!(store.load().await.is_none());

        manager
            .reauthenticate(account.id, "pw2")
            .await
            .expect("reauth");
        assert_eq!(store.load().await.as_deref(), Some("rt_a@example.com"));
    }

    #[tokio::test]
    async fn editing_an_account_keeps_the_unspecified_fields() {
        let (_dir, _db, manager) = manager().await;
        let server = server_with_login().await;
        let discovery = discovery_for(&server);
        let account = manager
            .add_discovered("a@example.com", "pw", &discovery, Some("Old".into()))
            .await
            .expect("add");

        let edited = manager
            .edit(account.id, Some(Some("New".into())), Some(Some(30)), None)
            .await
            .expect("edit");
        assert_eq!(edited.display_name.as_deref(), Some("New"));
        assert_eq!(edited.sync_window_days, Some(30));
        assert_eq!(edited.cache_limit_bytes, None);

        let cleared = manager
            .edit(account.id, None, Some(None), None)
            .await
            .expect("edit");
        assert_eq!(cleared.display_name.as_deref(), Some("New"));
        assert_eq!(cleared.sync_window_days, None);
    }

    #[tokio::test]
    async fn the_device_uid_is_stable_across_calls_and_instances() {
        let (_dir, db, manager) = manager().await;
        let first = manager.device_uid().await.expect("uid");
        let second = manager.device_uid().await.expect("uid");
        assert_eq!(first, second);
        assert!(first.starts_with("dev_"));

        let other = AccountManager::new(db).expect("manager");
        assert_eq!(other.device_uid().await.expect("uid"), first);
    }

    #[tokio::test]
    async fn the_status_view_counts_queued_work() {
        let (_dir, _db, manager) = manager().await;
        let server = server_with_login().await;
        let discovery = discovery_for(&server);
        let account = manager
            .add_discovered("a@example.com", "pw", &discovery, None)
            .await
            .expect("add");

        manager
            .queue_operation(
                account.id,
                crate::operations::OperationKind::MarkRead,
                serde_json::json!({"message_id": 5}),
            )
            .await
            .expect("queue");
        sqlx::query(
            "INSERT INTO sync_state (account_id, folder_id, cursor, status) VALUES (?, 5, '1841', 'idle')",
        )
        .bind(account.id.get())
        .execute(manager.database().pool())
        .await
        .expect("stream");

        let status = manager.status(account.id).await.expect("status");
        assert_eq!(status.pending_operations, 1);
        assert_eq!(status.dirty_drafts, 0);
        assert_eq!(status.streams.len(), 1);
        assert_eq!(status.streams[0].cursor, "1841");
        assert_eq!(status.summary(), "1 operation(s) pending");
    }

    #[tokio::test]
    async fn the_status_view_reports_paused_and_errors() {
        let (_dir, _db, manager) = manager().await;
        let server = server_with_login().await;
        let discovery = discovery_for(&server);
        let account = manager
            .add_discovered("a@example.com", "pw", &discovery, None)
            .await
            .expect("add");
        manager.pause(account.id, true).await.expect("pause");
        assert_eq!(manager.status(account.id).await.expect("status").summary(), "paused");

        manager.pause(account.id, false).await.expect("resume");
        manager
            .record_sync(account.id, Some("connection reset"))
            .await
            .expect("record");
        let status = manager.status(account.id).await.expect("status");
        assert!(status.summary().starts_with("error:"));
        assert_eq!(status.last_error.as_deref(), Some("connection reset"));

        manager.record_sync(account.id, None).await.expect("record");
        assert_eq!(manager.status(account.id).await.expect("status").summary(), "up to date");
    }

    #[tokio::test]
    async fn a_bad_address_is_rejected_before_any_network_call() {
        let (_dir, _db, manager) = manager().await;
        let server = server_with_login().await;
        let discovery = discovery_for(&server);
        let err = manager
            .add_discovered("not an address", "pw", &discovery, None)
            .await
            .expect_err("must refuse");
        assert!(matches!(err, ClientError::Invalid(_)));
        assert!(manager.list().await.expect("list").is_empty());
    }

    #[tokio::test]
    async fn a_failed_login_does_not_leave_a_broken_account_behind() {
        let (_dir, _db, manager) = manager().await;
        let server = MockServer::start().await;
        server.error_route("POST", "/api/v1/client/auth/login", 401, "unauthorized", "wrong password");
        let discovery = discovery_for(&server);
        let err = manager
            .add_discovered("a@example.com", "pw", &discovery, None)
            .await
            .expect_err("must fail");
        assert_eq!(err.api_status(), Some(401));

        // The row exists (so the user can retry) but carries no credentials.
        let listed = manager.list().await.expect("list");
        assert_eq!(listed.len(), 1);
        assert!(!listed[0].has_credentials());
        assert!(!listed[0].is_syncable());
    }

    #[tokio::test]
    async fn adding_the_same_account_twice_is_refused_by_the_unique_index() {
        let (_dir, _db, manager) = manager().await;
        let server = server_with_login().await;
        let discovery = discovery_for(&server);
        manager
            .add_discovered("a@example.com", "pw", &discovery, None)
            .await
            .expect("first");
        let err = manager
            .add_discovered("a@example.com", "pw", &discovery, None)
            .await
            .expect_err("second must fail");
        assert!(matches!(err, ClientError::Database(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn the_explicit_server_path_skips_autodiscovery() {
        let (_dir, _db, manager) = manager().await;
        let server = server_with_login().await;
        let account = manager
            .add(
                "a@example.com",
                "pw",
                Some("Mine".into()),
                Some(format!("{}/api/v1/client", server.base_url())),
            )
            .await
            .expect("add");
        assert_eq!(account.email, "a@example.com");
        assert!(account.base_url.starts_with("http://127.0.0.1"));
        assert_eq!(server.count_for("/.well-known/ferroma"), 0);
    }

    #[tokio::test]
    async fn a_client_for_an_account_reads_that_accounts_token() {
        let (_dir, _db, manager) = manager().await;
        let server = server_with_login().await;
        let discovery = discovery_for(&server);
        let account = manager
            .add_discovered("a@example.com", "pw", &discovery, None)
            .await
            .expect("add");
        let client = manager.client(account.id).await.expect("client");
        assert!(client.can_refresh().await);
        assert_eq!(client.base_url(), account.base_url.trim_end_matches('/'));
    }

    #[tokio::test]
    async fn queuing_for_an_unknown_account_is_not_found() {
        let (_dir, _db, manager) = manager().await;
        let err = manager
            .queue_operation(
                AccountId(42),
                crate::operations::OperationKind::Star,
                serde_json::json!({"message_id": 1}),
            )
            .await
            .expect_err("must fail");
        assert!(matches!(err, ClientError::NotFound(_)));
    }
}
