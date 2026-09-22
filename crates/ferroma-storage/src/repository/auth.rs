//! `sessions` and `devices` — who is logged in, and from where.
//!
//! A session stores only the SHA-256 of its token; the token itself never reaches the
//! database, so a dump of `sessions` cannot be replayed. A device is the stable
//! identity of an installed client, which survives re-logins and is what the
//! "signed-in devices" screen in the client actually lists.

use chrono::{DateTime, Utc};
use sqlx::PgPool;

use ferroma_core::{DeviceId, SessionId, UserId};

use crate::error::{Result, StorageError};
use crate::models::{Device, Session};
use crate::repository::{not_found, unique_conflict};

/// Everything needed to open a session.
#[derive(Debug, Clone)]
pub struct NewSession {
    /// The account that logged in.
    pub user_id: UserId,
    /// `web`, `api`, `client`, `imap` or `smtp`.
    pub kind: String,
    /// SHA-256 of the opaque token. The raw token is never stored.
    pub token_hash: String,
    /// The device the session belongs to, for client sessions.
    pub device_id: Option<DeviceId>,
    /// The peer address the session was opened from.
    pub ip: Option<String>,
    /// The `User-Agent` that opened it.
    pub user_agent: Option<String>,
    /// When it stops being valid.
    pub expires_at: DateTime<Utc>,
}

/// Web/API/client sessions.
#[derive(Debug, Clone)]
pub struct SessionsRepository {
    pool: PgPool,
}

impl SessionsRepository {
    /// Build the repository over `pool`.
    pub(crate) fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// The pool this repository queries.
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Open a session.
    pub async fn create(&self, new: NewSession) -> Result<Session> {
        sqlx::query_as::<_, Session>(
            "INSERT INTO sessions (user_id, kind, token_hash, device_id, ip, user_agent, expires_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             RETURNING *",
        )
        .bind(new.user_id.get())
        .bind(&new.kind)
        .bind(&new.token_hash)
        .bind(new.device_id.map(DeviceId::get))
        .bind(new.ip.as_deref())
        .bind(new.user_agent.as_deref())
        .bind(new.expires_at)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| unique_conflict(e.into(), "session token"))
    }

    /// Resolve a token hash to its session. Revoked and expired sessions are returned
    /// too — the caller checks [`crate::models::Session::is_valid_at`], so it can tell
    /// "wrong token" from "your session expired".
    pub async fn find_by_token_hash(&self, token_hash: &str) -> Result<Option<Session>> {
        Ok(
            sqlx::query_as::<_, Session>("SELECT * FROM sessions WHERE token_hash = $1")
                .bind(token_hash)
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    /// Look a session up by id.
    pub async fn find_by_id(&self, id: SessionId) -> Result<Option<Session>> {
        Ok(sqlx::query_as::<_, Session>("SELECT * FROM sessions WHERE id = $1")
            .bind(id.get())
            .fetch_optional(&self.pool)
            .await?)
    }

    /// Stamp `last_seen_at`, and the peer address when one was supplied.
    pub async fn touch(&self, id: SessionId, ip: Option<&str>) -> Result<()> {
        let done = sqlx::query(
            "UPDATE sessions SET last_seen_at = NOW(), ip = COALESCE($2::TEXT, ip) WHERE id = $1",
        )
        .bind(id.get())
        .bind(ip)
        .execute(&self.pool)
        .await?;
        if done.rows_affected() == 0 {
            return Err(not_found(format!("session {id}")));
        }
        Ok(())
    }

    /// Revoke one session. Returns `false` when it was already revoked or unknown.
    pub async fn revoke(&self, id: SessionId) -> Result<bool> {
        let done =
            sqlx::query("UPDATE sessions SET revoked_at = NOW() WHERE id = $1 AND revoked_at IS NULL")
                .bind(id.get())
                .execute(&self.pool)
                .await?;
        Ok(done.rows_affected() > 0)
    }

    /// Revoke every live session of an account — "sign out everywhere".
    pub async fn revoke_all_for_user(&self, user_id: UserId) -> Result<u64> {
        let done = sqlx::query(
            "UPDATE sessions SET revoked_at = NOW() WHERE user_id = $1 AND revoked_at IS NULL",
        )
        .bind(user_id.get())
        .execute(&self.pool)
        .await?;
        Ok(done.rows_affected())
    }

    /// Revoke every live session of a device — used when a client is unlinked.
    pub async fn revoke_for_device(&self, device_id: DeviceId) -> Result<u64> {
        let done = sqlx::query(
            "UPDATE sessions SET revoked_at = NOW() WHERE device_id = $1 AND revoked_at IS NULL",
        )
        .bind(device_id.get())
        .execute(&self.pool)
        .await?;
        Ok(done.rows_affected())
    }

    /// The newest still-usable session of one kind for an account.
    ///
    /// A JMAP client authenticates with its password on every request. Opening a
    /// session each time would write a row per request; this finds the one already
    /// open for that device so the caller can reuse it.
    pub async fn find_live(
        &self,
        user_id: UserId,
        kind: &str,
        device_id: Option<i64>,
        now: DateTime<Utc>,
    ) -> Result<Option<Session>> {
        Ok(sqlx::query_as::<_, Session>(
            "SELECT * FROM sessions
              WHERE user_id = $1 AND kind = $2
                AND revoked_at IS NULL AND expires_at > $3
                AND ($4::BIGINT IS NULL OR device_id = $4)
              ORDER BY id ASC
              LIMIT 1",
        )
        .bind(user_id.get())
        .bind(kind)
        .bind(now)
        .bind(device_id)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// An account's sessions, newest first.
    pub async fn list_for_user(
        &self,
        user_id: UserId,
        include_revoked: bool,
    ) -> Result<Vec<Session>> {
        Ok(sqlx::query_as::<_, Session>(
            "SELECT * FROM sessions
              WHERE user_id = $1 AND ($2 OR revoked_at IS NULL)
              ORDER BY created_at DESC, id DESC",
        )
        .bind(user_id.get())
        .bind(include_revoked)
        .fetch_all(&self.pool)
        .await?)
    }

    /// How many sessions are currently usable.
    pub async fn count_active(&self) -> Result<i64> {
        let (count,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM sessions WHERE revoked_at IS NULL AND expires_at > NOW()",
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(count)
    }

    /// Delete expired and revoked sessions.
    ///
    /// Revoked rows carry no information once the audit trail has seen them, and
    /// leaving them behind would let the table grow without bound on a busy server.
    pub async fn delete_expired(&self, now: DateTime<Utc>) -> Result<u64> {
        let done = sqlx::query("DELETE FROM sessions WHERE expires_at <= $1 OR revoked_at IS NOT NULL")
            .bind(now)
            .execute(&self.pool)
            .await?;
        Ok(done.rows_affected())
    }
}

/// Everything needed to register or refresh a device.
#[derive(Debug, Clone)]
pub struct DeviceUpsert {
    /// The owning account.
    pub user_id: UserId,
    /// The stable identifier generated by the client installation.
    pub device_uid: String,
    /// Human-readable device name.
    pub name: Option<String>,
    /// `windows`, `linux`, `macos`, `android` or `ios`.
    pub platform: Option<String>,
    /// The client's own version.
    pub client_version: Option<String>,
    /// The protocol revision the client speaks.
    pub protocol_version: Option<i32>,
    /// The address the device was last seen from.
    pub ip: Option<String>,
}

/// Official client installations.
#[derive(Debug, Clone)]
pub struct DevicesRepository {
    pool: PgPool,
}

impl DevicesRepository {
    /// Build the repository over `pool`.
    pub(crate) fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// The pool this repository queries.
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Register a device, or refresh the one already known under this `device_uid`.
    ///
    /// A `None` field never erases a value the server already knows: a client that
    /// omits its version on one request should not lose it. Re-registering a revoked
    /// device clears `revoked_at`, because presenting a fresh login for the same
    /// installation is exactly what re-authorising means.
    pub async fn upsert(&self, upsert: DeviceUpsert) -> Result<Device> {
        let device_uid = upsert.device_uid.trim();
        if device_uid.is_empty() {
            return Err(StorageError::Invalid("device_uid must not be blank".into()));
        }

        Ok(sqlx::query_as::<_, Device>(
            "INSERT INTO devices (user_id, device_uid, name, platform, client_version,
                                  protocol_version, last_ip, last_seen_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, NOW())
             ON CONFLICT (user_id, device_uid) DO UPDATE
                SET name = COALESCE(EXCLUDED.name, devices.name),
                    platform = COALESCE(EXCLUDED.platform, devices.platform),
                    client_version = COALESCE(EXCLUDED.client_version, devices.client_version),
                    protocol_version = COALESCE(EXCLUDED.protocol_version, devices.protocol_version),
                    last_ip = COALESCE(EXCLUDED.last_ip, devices.last_ip),
                    last_seen_at = NOW(),
                    revoked_at = NULL
             RETURNING *",
        )
        .bind(upsert.user_id.get())
        .bind(device_uid)
        .bind(upsert.name.as_deref())
        .bind(upsert.platform.as_deref())
        .bind(upsert.client_version.as_deref())
        .bind(upsert.protocol_version)
        .bind(upsert.ip.as_deref())
        .fetch_one(&self.pool)
        .await?)
    }

    /// Look a device up by id.
    pub async fn find_by_id(&self, id: DeviceId) -> Result<Option<Device>> {
        Ok(sqlx::query_as::<_, Device>("SELECT * FROM devices WHERE id = $1")
            .bind(id.get())
            .fetch_optional(&self.pool)
            .await?)
    }

    /// Look a device up by its client-generated identifier.
    pub async fn find_by_uid(&self, user_id: UserId, device_uid: &str) -> Result<Option<Device>> {
        Ok(sqlx::query_as::<_, Device>(
            "SELECT * FROM devices WHERE user_id = $1 AND device_uid = $2",
        )
        .bind(user_id.get())
        .bind(device_uid.trim())
        .fetch_optional(&self.pool)
        .await?)
    }

    /// An account's devices, newest first.
    pub async fn list_for_user(
        &self,
        user_id: UserId,
        include_revoked: bool,
    ) -> Result<Vec<Device>> {
        Ok(sqlx::query_as::<_, Device>(
            "SELECT * FROM devices
              WHERE user_id = $1 AND ($2 OR revoked_at IS NULL)
              ORDER BY created_at DESC, id DESC",
        )
        .bind(user_id.get())
        .bind(include_revoked)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Every device that may still sync, most recently seen first.
    pub async fn list_active(&self) -> Result<Vec<Device>> {
        Ok(sqlx::query_as::<_, Device>(
            "SELECT * FROM devices WHERE revoked_at IS NULL
              ORDER BY last_seen_at DESC NULLS LAST, id DESC",
        )
        .fetch_all(&self.pool)
        .await?)
    }

    /// Stamp `last_seen_at`, and the peer address when one was supplied.
    pub async fn touch(&self, id: DeviceId, ip: Option<&str>) -> Result<()> {
        let done = sqlx::query(
            "UPDATE devices SET last_seen_at = NOW(), last_ip = COALESCE($2::TEXT, last_ip)
              WHERE id = $1",
        )
        .bind(id.get())
        .bind(ip)
        .execute(&self.pool)
        .await?;
        if done.rows_affected() == 0 {
            return Err(not_found(format!("device {id}")));
        }
        Ok(())
    }

    /// Revoke a device. Returns `false` when it was already revoked or unknown.
    pub async fn revoke(&self, id: DeviceId) -> Result<bool> {
        let done =
            sqlx::query("UPDATE devices SET revoked_at = NOW() WHERE id = $1 AND revoked_at IS NULL")
                .bind(id.get())
                .execute(&self.pool)
                .await?;
        Ok(done.rows_affected() > 0)
    }

    /// Forget a device entirely. Its sessions keep their (now `NULL`) device link.
    pub async fn delete(&self, id: DeviceId) -> Result<bool> {
        let done = sqlx::query("DELETE FROM devices WHERE id = $1")
            .bind(id.get())
            .execute(&self.pool)
            .await?;
        Ok(done.rows_affected() > 0)
    }

    /// How many devices of one account may still sync.
    pub async fn count_active_for_user(&self, user_id: UserId) -> Result<i64> {
        let (count,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM devices WHERE user_id = $1 AND revoked_at IS NULL",
        )
        .bind(user_id.get())
        .fetch_one(&self.pool)
        .await?;
        Ok(count)
    }
}
