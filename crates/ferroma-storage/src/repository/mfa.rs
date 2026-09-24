//! Second factors and client credentials: TOTP, recovery codes, app passwords.
//!
//! Three tables from `0011_mfa.sql`, three repositories. They belong to one
//! aggregate — an account has a confirmed TOTP registration and a set of recovery
//! codes, or it has neither — but each answers a question a different caller asks,
//! so they are kept apart:
//!
//! * [`TotpRepository`] — the shared secret, and whether the user has proved that
//!   their authenticator produces codes from it.
//! * [`RecoveryCodesRepository`] — the single-use codes, whose whole lifecycle is
//!   "generate a set, spend one at a time".
//! * [`AppPasswordsRepository`] — long-lived credentials for one mail client. They
//!   are an *alternative* to the password, not a second factor, so they are created,
//!   verified, listed and revoked per account.
//!
//! # Nothing here hashes or generates anything
//!
//! A caller hands in a base32 secret, or the hash of a code, and gets back a row or
//! a boolean. Hashing and code generation belong next to the thing that verifies
//! them (`ferroma-auth`): keeping them out of storage means no plaintext secret or
//! token is ever bound into a query, and storage never picks a hash algorithm on
//! the auth layer's behalf.
//!
//! # Single-use means a conditional `UPDATE`
//!
//! [`RecoveryCodesRepository::consume`] and [`AppPasswordsRepository::verify`] spend
//! and stamp a row with one statement that filters on the state they require
//! (`used_at IS NULL`, `revoked_at IS NULL`). Two concurrent logins therefore race on
//! the row lock, not in the client: exactly one of them sees a row change, which is
//! what makes a recovery code and a revoked app password behave the way the operator
//! documented.

use chrono::{DateTime, Utc};
use ferroma_core::UserId;
use sqlx::PgPool;

use crate::error::{Result, StorageError};
use crate::repository::unique_conflict;

// ===========================================================================
// TOTP
// ===========================================================================

/// One account's TOTP enrolment.
///
/// The two states are told apart by [`confirmed_at`](Self::confirmed_at): `None`
/// means the secret exists but the user has not yet proved it works, and only a
/// timestamp means the account is actually protected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TotpEnrollment {
    /// The shared secret, base32 and without padding.
    pub secret: String,
    /// When the user confirmed the secret, or `None` while it is unconfirmed.
    pub confirmed_at: Option<DateTime<Utc>>,
}

/// The `user_totp` table: at most one registration per account.
#[derive(Debug, Clone)]
pub struct TotpRepository {
    pool: PgPool,
}

impl TotpRepository {
    /// Build the repository over `pool`.
    pub(crate) fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// The pool this repository queries.
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Write the account's secret, replacing one that is there.
    ///
    /// The replacement is always **unconfirmed**: a fresh secret has not been proved
    /// yet, so leaving an old `confirmed_at` in place would let an enrolment in
    /// progress inherit the previous registration's confirmation. Recovery codes are
    /// untouched — they belong to the completed enrolment, and the caller dealing
    /// with the confirmation decides when they are replaced.
    ///
    /// A blank secret is refused rather than stored: it would satisfy `NOT NULL` and
    /// protect nothing.
    pub async fn upsert_secret(&self, user_id: UserId, secret: &str) -> Result<()> {
        let secret = secret.trim();
        if secret.is_empty() {
            return Err(StorageError::Invalid(format!(
                "user {} cannot enrol TOTP with a blank secret",
                user_id.get()
            )));
        }

        sqlx::query(
            "INSERT INTO user_totp (user_id, secret)
             VALUES ($1, $2)
             ON CONFLICT (user_id) DO UPDATE
                SET secret = EXCLUDED.secret,
                    confirmed_at = NULL,
                    created_at = NOW()",
        )
        .bind(user_id.get())
        .bind(secret)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// The account's registration, confirmed or not.
    pub async fn find(&self, user_id: UserId) -> Result<Option<TotpEnrollment>> {
        let row: Option<(String, Option<DateTime<Utc>>)> =
            sqlx::query_as("SELECT secret, confirmed_at FROM user_totp WHERE user_id = $1")
                .bind(user_id.get())
                .fetch_optional(&self.pool)
                .await?;

        Ok(row.map(|(secret, confirmed_at)| TotpEnrollment {
            secret,
            confirmed_at,
        }))
    }

    /// Mark an unconfirmed registration as confirmed.
    ///
    /// Confirmation is one-way and idempotent by refusal: only a row that is still
    /// unconfirmed can be confirmed, so the returned flag is `false` when there is no
    /// registration to confirm *or* when this one already was. `false` therefore means
    /// "nothing changed", which is exactly what a caller that has just verified a code
    /// needs to know before it claims the account is protected.
    pub async fn confirm(&self, user_id: UserId, at: DateTime<Utc>) -> Result<bool> {
        let done = sqlx::query(
            "UPDATE user_totp
                SET confirmed_at = $2
              WHERE user_id = $1 AND confirmed_at IS NULL",
        )
        .bind(user_id.get())
        .bind(at)
        .execute(&self.pool)
        .await?;

        Ok(done.rows_affected() > 0)
    }

    /// Turn two-factor authentication off: the secret and the recovery codes go
    /// together.
    ///
    /// They are removed in one transaction because recovery codes without a secret
    /// are a way into an account that the operator believes is protected by nothing
    /// but a password. The return value reports whether a secret was there to delete;
    /// recovery codes are cleared either way, so a leftover set from a half-finished
    /// enrolment cannot survive.
    pub async fn delete(&self, user_id: UserId) -> Result<bool> {
        let mut tx = self.pool.begin().await?;

        sqlx::query("DELETE FROM totp_recovery_codes WHERE user_id = $1")
            .bind(user_id.get())
            .execute(&mut *tx)
            .await?;

        let done = sqlx::query("DELETE FROM user_totp WHERE user_id = $1")
            .bind(user_id.get())
            .execute(&mut *tx)
            .await?;

        tx.commit().await?;
        Ok(done.rows_affected() > 0)
    }
}

// ===========================================================================
// Recovery codes
// ===========================================================================

/// One recovery-code row: its identity and whether it has been spent.
///
/// The hash is deliberately not part of the row image. It is only ever an input —
/// bound into [`RecoveryCodesRepository::consume`] — so nothing that reads a code
/// back out of storage can compare or log one.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct RecoveryCode {
    /// The row's identifier.
    pub id: i64,
    /// When the code was spent, or `None` while it is still usable.
    pub used_at: Option<DateTime<Utc>>,
}

/// The `totp_recovery_codes` table.
#[derive(Debug, Clone)]
pub struct RecoveryCodesRepository {
    pool: PgPool,
}

impl RecoveryCodesRepository {
    /// Build the repository over `pool`.
    pub(crate) fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// The pool this repository queries.
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Replace every recovery code the account has with `code_hashes`.
    ///
    /// One transaction, delete then insert: a caller that fails halfway leaves the
    /// previous set intact rather than an account with no codes or with a mixture.
    /// An empty slice is a valid replacement and leaves the account with none — that
    /// is what regenerating without storing does, and it is the caller's problem, not
    /// a silent no-op. Duplicate hashes in the slice are rejected by the uniqueness
    /// constraint rather than stored twice, which is a caller bug worth surfacing.
    pub async fn replace_all(&self, user_id: UserId, code_hashes: &[String]) -> Result<()> {
        if code_hashes.iter().any(|hash| hash.trim().is_empty()) {
            return Err(StorageError::Invalid(format!(
                "user {} cannot store a blank recovery code hash",
                user_id.get()
            )));
        }

        let mut tx = self.pool.begin().await?;

        sqlx::query("DELETE FROM totp_recovery_codes WHERE user_id = $1")
            .bind(user_id.get())
            .execute(&mut *tx)
            .await?;

        sqlx::query(
            "INSERT INTO totp_recovery_codes (user_id, code_hash)
             SELECT $1, code FROM unnest($2::text[]) AS codes (code)",
        )
        .bind(user_id.get())
        .bind(code_hashes)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(())
    }

    /// Spend one unused recovery code.
    ///
    /// The `used_at IS NULL` predicate is the whole mechanism: the `UPDATE` takes the
    /// row lock, so of two requests presenting the same code exactly one changes a row
    /// and the other gets `false`. An unknown code, another account's code and one
    /// already spent are all indistinguishable here — all `false` — which is what a
    /// caller wants, since telling them apart would leak whether a hash is live.
    pub async fn consume(&self, user_id: UserId, code_hash: &str) -> Result<bool> {
        let done = sqlx::query(
            "UPDATE totp_recovery_codes
                SET used_at = NOW()
              WHERE user_id = $1 AND code_hash = $2 AND used_at IS NULL",
        )
        .bind(user_id.get())
        .bind(code_hash)
        .execute(&self.pool)
        .await?;

        Ok(done.rows_affected() > 0)
    }

    /// How many recovery codes the account can still spend.
    pub async fn count_unused(&self, user_id: UserId) -> Result<i64> {
        let (count,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM totp_recovery_codes
              WHERE user_id = $1 AND used_at IS NULL",
        )
        .bind(user_id.get())
        .fetch_one(&self.pool)
        .await?;

        Ok(count)
    }

    /// Every code the account has, spent or not, oldest first.
    ///
    /// Spent rows are kept so the account can see which of its printed codes have
    /// been used; the caller decides whether that is worth showing.
    pub async fn list(&self, user_id: UserId) -> Result<Vec<RecoveryCode>> {
        let rows = sqlx::query_as::<_, RecoveryCode>(
            "SELECT id, used_at FROM totp_recovery_codes
              WHERE user_id = $1
              ORDER BY id",
        )
        .bind(user_id.get())
        .fetch_all(&self.pool)
        .await?;

        Ok(rows)
    }
}

// ===========================================================================
// App passwords
// ===========================================================================

/// One app password's metadata.
///
/// Never the token, and never its hash: this is what an account's settings page
/// lists, and it has to be safe to serialise to that page.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct AppPassword {
    /// The row's identifier, which is what a revoke call names.
    pub id: i64,
    /// The owning account, as the raw key of the row.
    pub user_id: i64,
    /// The name the user gave it, e.g. `Phone (Thunderbird)`.
    pub label: String,
    /// When it was created.
    pub created_at: DateTime<Utc>,
    /// When it was last presented, or `None` while it has never been used.
    pub last_used_at: Option<DateTime<Utc>>,
    /// When it was revoked, or `None` while it still works.
    pub revoked_at: Option<DateTime<Utc>>,
}

/// The `app_passwords` table.
#[derive(Debug, Clone)]
pub struct AppPasswordsRepository {
    pool: PgPool,
}

impl AppPasswordsRepository {
    /// Build the repository over `pool`.
    pub(crate) fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// The pool this repository queries.
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Store a new app password and return its metadata.
    ///
    /// `token_hash` is globally unique, because the token is presented without a
    /// username: the hash alone has to identify the row. A collision is therefore a
    /// [`StorageError::Conflict`], not a second row for the same token. A blank label
    /// or a blank hash is refused — the label is how the user recognises the
    /// credential later, and a blank hash would match a blank lookup.
    pub async fn create(
        &self,
        user_id: UserId,
        label: &str,
        token_hash: &str,
    ) -> Result<AppPassword> {
        let label = label.trim();
        if label.is_empty() {
            return Err(StorageError::Invalid(format!(
                "an app password for user {} needs a label",
                user_id.get()
            )));
        }
        if token_hash.trim().is_empty() {
            return Err(StorageError::Invalid(format!(
                "an app password for user {} cannot have a blank token hash",
                user_id.get()
            )));
        }

        let row = sqlx::query_as::<_, AppPassword>(
            "INSERT INTO app_passwords (user_id, label, token_hash)
             VALUES ($1, $2, $3)
             RETURNING id, user_id, label, created_at, last_used_at, revoked_at",
        )
        .bind(user_id.get())
        .bind(label)
        .bind(token_hash)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| unique_conflict(e.into(), "app password token"))?;

        Ok(row)
    }

    /// Verify a presented token and stamp it as used.
    ///
    /// One statement does both, filtered on `revoked_at IS NULL`, so a revoked
    /// credential cannot be brought back to life by a concurrent request and a
    /// successful verify always leaves `last_used_at` set. `false` covers every
    /// failure the same way — wrong hash, another account's token, revoked — and the
    /// account is part of the lookup, so one user's token can never authenticate
    /// another.
    pub async fn verify(&self, user_id: UserId, token_hash: &str) -> Result<bool> {
        let done = sqlx::query(
            "UPDATE app_passwords
                SET last_used_at = NOW()
              WHERE user_id = $1 AND token_hash = $2 AND revoked_at IS NULL",
        )
        .bind(user_id.get())
        .bind(token_hash)
        .execute(&self.pool)
        .await?;

        Ok(done.rows_affected() > 0)
    }

    /// Every app password the account has, revoked ones included, oldest first.
    ///
    /// Revoked rows stay in the list on purpose: the entry is what tells the user
    /// that the credential they no longer recognise was indeed revoked, and when.
    pub async fn list(&self, user_id: UserId) -> Result<Vec<AppPassword>> {
        let rows = sqlx::query_as::<_, AppPassword>(
            "SELECT id, user_id, label, created_at, last_used_at, revoked_at
               FROM app_passwords
              WHERE user_id = $1
              ORDER BY id",
        )
        .bind(user_id.get())
        .fetch_all(&self.pool)
        .await?;

        Ok(rows)
    }

    /// Revoke one app password.
    ///
    /// Scoped by owner, so an identifier from another account's list cannot revoke
    /// it, and filtered on `revoked_at IS NULL`, so the returned flag says whether
    /// *this* call revoked something. A second revoke is `false`: the credential was
    /// already dead, and the timestamp of the first revoke is what stays on the row.
    pub async fn revoke(&self, user_id: UserId, id: i64) -> Result<bool> {
        let done = sqlx::query(
            "UPDATE app_passwords
                SET revoked_at = NOW()
              WHERE user_id = $1 AND id = $2 AND revoked_at IS NULL",
        )
        .bind(user_id.get())
        .bind(id)
        .execute(&self.pool)
        .await?;

        Ok(done.rows_affected() > 0)
    }
}
