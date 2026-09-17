//! `users` — login identities, quotas and login-failure bookkeeping.
//!
//! The `users` table is the root of every ownership chain in the schema, so this
//! repository also owns the two counters that guard an account: `failed_logins`
//! (with its temporary lockout) and `used_bytes` (the cached quota figure).

use chrono::{DateTime, Utc};
use sqlx::PgPool;

use ferroma_core::UserId;

use crate::error::{Result, StorageError};
use crate::models::User;
use crate::repository::{limit_of, normalise, not_found, offset_of, unique_conflict};

/// The account quota used when [`NewUser::quota_bytes`] is `None`, matching the
/// `users.quota_bytes` column default.
const DEFAULT_QUOTA_BYTES: i64 = 1_073_741_824;

/// Everything needed to create a login identity.
#[derive(Debug, Clone)]
pub struct NewUser {
    /// Login address. Stored lower-cased and trimmed.
    pub email: String,
    /// Argon2id PHC string. Never logged and never returned by the API.
    pub password_hash: String,
    /// Optional human name.
    pub display_name: Option<String>,
    /// Grants access to the Admin API.
    pub is_admin: bool,
    /// Total bytes the account may store. `None` uses the schema default (1 GiB).
    pub quota_bytes: Option<i64>,
}

/// Login identities.
#[derive(Debug, Clone)]
pub struct UsersRepository {
    pool: PgPool,
}

impl UsersRepository {
    /// Build the repository over `pool`.
    pub(crate) fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// The pool this repository queries.
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Insert a new identity.
    ///
    /// The address is lower-cased before it is stored, so `Alice@Example.COM` and
    /// `alice@example.com` are the same account. A second insert of an existing
    /// address yields [`StorageError::Conflict`] rather than a raw `23505`.
    pub async fn create(&self, new: NewUser) -> Result<User> {
        let email = normalise(&new.email);
        if email.is_empty() {
            return Err(StorageError::Invalid("user email must not be blank".into()));
        }
        if new.quota_bytes.is_some_and(|q| q < 0) {
            return Err(StorageError::Invalid("user quota_bytes must be >= 0".into()));
        }

        sqlx::query_as::<_, User>(
            "INSERT INTO users (email, password_hash, display_name, is_admin, quota_bytes)
             VALUES ($1, $2, $3, $4, COALESCE($5::BIGINT, $6))
             RETURNING *",
        )
        .bind(&email)
        .bind(&new.password_hash)
        .bind(new.display_name.as_deref())
        .bind(new.is_admin)
        .bind(new.quota_bytes)
        .bind(DEFAULT_QUOTA_BYTES)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| unique_conflict(e.into(), format!("user {email}")))
    }

    /// Look an identity up by primary key.
    pub async fn find_by_id(&self, id: UserId) -> Result<Option<User>> {
        Ok(sqlx::query_as::<_, User>("SELECT * FROM users WHERE id = $1")
            .bind(id.get())
            .fetch_optional(&self.pool)
            .await?)
    }

    /// Look an identity up by login address. Case-insensitive.
    pub async fn find_by_email(&self, email: &str) -> Result<Option<User>> {
        Ok(
            sqlx::query_as::<_, User>("SELECT * FROM users WHERE email = $1")
                .bind(normalise(email))
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    /// [`UsersRepository::find_by_id`], but a missing row is an error.
    pub async fn require_by_id(&self, id: UserId) -> Result<User> {
        self.find_by_id(id)
            .await?
            .ok_or_else(|| not_found(format!("user {id}")))
    }

    /// [`UsersRepository::find_by_email`], but a missing row is an error.
    pub async fn require_by_email(&self, email: &str) -> Result<User> {
        self.find_by_email(email)
            .await?
            .ok_or_else(|| not_found(format!("user {email}")))
    }

    /// A page of identities, newest first.
    pub async fn list(&self, limit: i64, offset: i64) -> Result<Vec<User>> {
        Ok(sqlx::query_as::<_, User>(
            "SELECT * FROM users ORDER BY created_at DESC, id DESC LIMIT $1 OFFSET $2",
        )
        .bind(limit_of(limit))
        .bind(offset_of(offset))
        .fetch_all(&self.pool)
        .await?)
    }

    /// How many identities exist.
    pub async fn count(&self) -> Result<i64> {
        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM users")
            .fetch_one(&self.pool)
            .await?;
        Ok(count)
    }

    /// How many identities hold the admin bit.
    pub async fn count_admins(&self) -> Result<i64> {
        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM users WHERE is_admin")
            .fetch_one(&self.pool)
            .await?;
        Ok(count)
    }

    /// Enable or disable every way of logging in.
    pub async fn set_enabled(&self, id: UserId, enabled: bool) -> Result<()> {
        let done = sqlx::query("UPDATE users SET enabled = $2, updated_at = NOW() WHERE id = $1")
            .bind(id.get())
            .bind(enabled)
            .execute(&self.pool)
            .await?;
        require_touched(done.rows_affected(), id)
    }

    /// Grant or revoke the admin bit.
    pub async fn set_admin(&self, id: UserId, is_admin: bool) -> Result<()> {
        let done = sqlx::query("UPDATE users SET is_admin = $2, updated_at = NOW() WHERE id = $1")
            .bind(id.get())
            .bind(is_admin)
            .execute(&self.pool)
            .await?;
        require_touched(done.rows_affected(), id)
    }

    /// Replace the display name (`None` clears it).
    pub async fn set_display_name(&self, id: UserId, name: Option<&str>) -> Result<()> {
        let done =
            sqlx::query("UPDATE users SET display_name = $2, updated_at = NOW() WHERE id = $1")
                .bind(id.get())
                .bind(name)
                .execute(&self.pool)
                .await?;
        require_touched(done.rows_affected(), id)
    }

    /// Replace the account quota.
    pub async fn set_quota(&self, id: UserId, quota_bytes: i64) -> Result<()> {
        if quota_bytes < 0 {
            return Err(StorageError::Invalid("user quota_bytes must be >= 0".into()));
        }
        let done = sqlx::query("UPDATE users SET quota_bytes = $2, updated_at = NOW() WHERE id = $1")
            .bind(id.get())
            .bind(quota_bytes)
            .execute(&self.pool)
            .await?;
        require_touched(done.rows_affected(), id)
    }

    /// Replace the password hash.
    pub async fn update_password(&self, id: UserId, password_hash: &str) -> Result<()> {
        let done =
            sqlx::query("UPDATE users SET password_hash = $2, updated_at = NOW() WHERE id = $1")
                .bind(id.get())
                .bind(password_hash)
                .execute(&self.pool)
                .await?;
        require_touched(done.rows_affected(), id)
    }

    /// Record a successful login: the failure counter and any lockout are cleared.
    pub async fn record_login_success(&self, id: UserId, when: DateTime<Utc>) -> Result<()> {
        let done = sqlx::query(
            "UPDATE users
                SET failed_logins = 0, locked_until = NULL, last_login_at = $2, updated_at = NOW()
              WHERE id = $1",
        )
        .bind(id.get())
        .bind(when)
        .execute(&self.pool)
        .await?;
        require_touched(done.rows_affected(), id)
    }

    /// Record a failed login, and lock the account once the threshold is reached.
    ///
    /// The counter is incremented in SQL, so two concurrent failures cannot lose an
    /// update. When the new count reaches `max_failures` the account is locked until
    /// `when + lockout_secs` — `max_failures == 0` therefore locks on the first
    /// failure, which is what a "no attempts allowed" policy means.
    ///
    /// Returns the updated row so the caller can report the remaining attempts.
    pub async fn record_login_failure(
        &self,
        id: UserId,
        when: DateTime<Utc>,
        lockout_secs: u64,
        max_failures: u32,
    ) -> Result<User> {
        sqlx::query_as::<_, User>(
            "UPDATE users
                SET failed_logins = failed_logins + 1,
                    locked_until = CASE
                        WHEN failed_logins + 1 >= $4
                        THEN $2::TIMESTAMPTZ + make_interval(secs => $3)
                        ELSE locked_until
                    END,
                    updated_at = NOW()
              WHERE id = $1
              RETURNING *",
        )
        .bind(id.get())
        .bind(when)
        .bind(lockout_secs as f64)
        .bind(i32::try_from(max_failures).unwrap_or(i32::MAX))
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| not_found(format!("user {id}")))
    }

    /// Add `delta_bytes` to the cached usage and return the new total.
    ///
    /// The result is clamped at zero: usage is a cache, and overshooting it by
    /// deleting a message twice must never leave an account with negative usage.
    pub async fn add_usage(&self, id: UserId, delta_bytes: i64) -> Result<i64> {
        let (used,): (i64,) = sqlx::query_as(
            "UPDATE users
                SET used_bytes = GREATEST(used_bytes + $2, 0), updated_at = NOW()
              WHERE id = $1
              RETURNING used_bytes",
        )
        .bind(id.get())
        .bind(delta_bytes)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| not_found(format!("user {id}")))?;
        Ok(used)
    }

    /// Delete an identity. Every owned row cascades away.
    ///
    /// Returns `false` when the identity did not exist.
    pub async fn delete(&self, id: UserId) -> Result<bool> {
        let done = sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(id.get())
            .execute(&self.pool)
            .await?;
        Ok(done.rows_affected() > 0)
    }
}

/// Turn "the `UPDATE` matched no row" into [`StorageError::NotFound`].
fn require_touched(rows: u64, id: UserId) -> Result<()> {
    if rows == 0 {
        return Err(not_found(format!("user {id}")));
    }
    Ok(())
}
