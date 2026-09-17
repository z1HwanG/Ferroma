//! `domains` and `aliases` — the two tables that decide which addresses exist.

use sqlx::PgPool;

use ferroma_core::DomainId;

use crate::error::{Result, StorageError};
use crate::models::{Alias, Domain};
use crate::repository::{normalise, not_found, unique_conflict};

/// The mail domains this server is authoritative for.
#[derive(Debug, Clone)]
pub struct DomainsRepository {
    pool: PgPool,
}

impl DomainsRepository {
    /// Build the repository over `pool`.
    pub(crate) fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// The pool this repository queries.
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Add a domain. The name is lower-cased; a duplicate is a
    /// [`StorageError::Conflict`].
    pub async fn create(&self, name: &str, description: Option<&str>) -> Result<Domain> {
        let name = normalise(name);
        if name.is_empty() {
            return Err(StorageError::Invalid("domain name must not be blank".into()));
        }

        sqlx::query_as::<_, Domain>(
            "INSERT INTO domains (name, description) VALUES ($1, $2) RETURNING *",
        )
        .bind(&name)
        .bind(description)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| unique_conflict(e.into(), format!("domain {name}")))
    }

    /// Look a domain up by primary key.
    pub async fn find_by_id(&self, id: DomainId) -> Result<Option<Domain>> {
        Ok(sqlx::query_as::<_, Domain>("SELECT * FROM domains WHERE id = $1")
            .bind(id.get())
            .fetch_optional(&self.pool)
            .await?)
    }

    /// Look a domain up by name. Case-insensitive.
    pub async fn find_by_name(&self, name: &str) -> Result<Option<Domain>> {
        Ok(
            sqlx::query_as::<_, Domain>("SELECT * FROM domains WHERE name = $1")
                .bind(normalise(name))
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    /// [`DomainsRepository::find_by_name`], but a missing domain is an error.
    pub async fn require_by_name(&self, name: &str) -> Result<Domain> {
        self.find_by_name(name)
            .await?
            .ok_or_else(|| not_found(format!("domain {name}")))
    }

    /// Every domain, alphabetical.
    pub async fn list(&self) -> Result<Vec<Domain>> {
        Ok(
            sqlx::query_as::<_, Domain>("SELECT * FROM domains ORDER BY name ASC")
                .fetch_all(&self.pool)
                .await?,
        )
    }

    /// Every enabled domain, alphabetical. The SMTP server accepts mail for these.
    pub async fn list_enabled(&self) -> Result<Vec<Domain>> {
        Ok(
            sqlx::query_as::<_, Domain>("SELECT * FROM domains WHERE enabled ORDER BY name ASC")
                .fetch_all(&self.pool)
                .await?,
        )
    }

    /// How many domains are configured.
    pub async fn count(&self) -> Result<i64> {
        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM domains")
            .fetch_one(&self.pool)
            .await?;
        Ok(count)
    }

    /// Enable or disable the domain (and therefore all of its addresses).
    pub async fn set_enabled(&self, id: DomainId, enabled: bool) -> Result<()> {
        let done = sqlx::query("UPDATE domains SET enabled = $2, updated_at = NOW() WHERE id = $1")
            .bind(id.get())
            .bind(enabled)
            .execute(&self.pool)
            .await?;
        touched(done.rows_affected(), id)
    }

    /// Replace the free-text description (`None` clears it).
    pub async fn set_description(&self, id: DomainId, description: Option<&str>) -> Result<()> {
        let done =
            sqlx::query("UPDATE domains SET description = $2, updated_at = NOW() WHERE id = $1")
                .bind(id.get())
                .bind(description)
                .execute(&self.pool)
                .await?;
        touched(done.rows_affected(), id)
    }

    /// Set (or clear) the catch-all local part.
    ///
    /// A bare local part, not a full address: the domain is implied by the row.
    pub async fn set_catch_all(&self, id: DomainId, local_part: Option<&str>) -> Result<()> {
        let normalised = local_part.map(normalise).filter(|s| !s.is_empty());
        let done =
            sqlx::query("UPDATE domains SET catch_all = $2, updated_at = NOW() WHERE id = $1")
                .bind(id.get())
                .bind(normalised.as_deref())
                .execute(&self.pool)
                .await?;
        touched(done.rows_affected(), id)
    }

    /// Replace the DKIM key material. Any of the three may be `None` to clear it.
    pub async fn set_dkim(
        &self,
        id: DomainId,
        selector: Option<&str>,
        private_key: Option<&str>,
        public_key: Option<&str>,
    ) -> Result<()> {
        let done = sqlx::query(
            "UPDATE domains
                SET dkim_selector = $2, dkim_private_key = $3, dkim_public_key = $4,
                    updated_at = NOW()
              WHERE id = $1",
        )
        .bind(id.get())
        .bind(selector)
        .bind(private_key)
        .bind(public_key)
        .execute(&self.pool)
        .await?;
        touched(done.rows_affected(), id)
    }

    /// Delete a domain. Its mailboxes and aliases cascade away.
    ///
    /// Returns `false` when the domain did not exist.
    pub async fn delete(&self, id: DomainId) -> Result<bool> {
        let done = sqlx::query("DELETE FROM domains WHERE id = $1")
            .bind(id.get())
            .execute(&self.pool)
            .await?;
        Ok(done.rows_affected() > 0)
    }
}

/// Forwarding aliases: `sales@example.com` -> `alice@example.com`.
#[derive(Debug, Clone)]
pub struct AliasesRepository {
    pool: PgPool,
}

impl AliasesRepository {
    /// Build the repository over `pool`.
    pub(crate) fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// The pool this repository queries.
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Create an alias for `local_part@<domain>`.
    ///
    /// `target` is either a full address or a bare local part, which means "the same
    /// domain". A duplicate local part in the same domain is a
    /// [`StorageError::Conflict`].
    pub async fn create(
        &self,
        domain_id: DomainId,
        local_part: &str,
        target: &str,
    ) -> Result<Alias> {
        let local_part = normalise(local_part);
        if local_part.is_empty() {
            return Err(StorageError::Invalid(
                "alias local_part must not be blank".into(),
            ));
        }
        let target = normalise(target);
        if target.is_empty() {
            return Err(StorageError::Invalid(
                "alias target must not be blank".into(),
            ));
        }

        sqlx::query_as::<_, Alias>(
            "INSERT INTO aliases (domain_id, local_part, target) VALUES ($1, $2, $3) RETURNING *",
        )
        .bind(domain_id.get())
        .bind(&local_part)
        .bind(&target)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| unique_conflict(e.into(), format!("alias {local_part}")))
    }

    /// Look an alias up by primary key.
    pub async fn find_by_id(&self, id: i64) -> Result<Option<Alias>> {
        Ok(sqlx::query_as::<_, Alias>("SELECT * FROM aliases WHERE id = $1")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?)
    }

    /// Look up an *enabled* alias by address. Case-insensitive.
    pub async fn find(&self, domain_id: DomainId, local_part: &str) -> Result<Option<Alias>> {
        Ok(sqlx::query_as::<_, Alias>(
            "SELECT * FROM aliases WHERE domain_id = $1 AND local_part = $2 AND enabled",
        )
        .bind(domain_id.get())
        .bind(normalise(local_part))
        .fetch_optional(&self.pool)
        .await?)
    }

    /// Every alias of one domain, alphabetical.
    pub async fn list_by_domain(&self, domain_id: DomainId) -> Result<Vec<Alias>> {
        Ok(sqlx::query_as::<_, Alias>(
            "SELECT * FROM aliases WHERE domain_id = $1 ORDER BY local_part ASC",
        )
        .bind(domain_id.get())
        .fetch_all(&self.pool)
        .await?)
    }

    /// Enable or disable one alias.
    pub async fn set_enabled(&self, id: i64, enabled: bool) -> Result<()> {
        let done = sqlx::query("UPDATE aliases SET enabled = $2 WHERE id = $1")
            .bind(id)
            .bind(enabled)
            .execute(&self.pool)
            .await?;
        if done.rows_affected() == 0 {
            return Err(not_found(format!("alias {id}")));
        }
        Ok(())
    }

    /// Point an alias somewhere else.
    pub async fn set_target(&self, id: i64, target: &str) -> Result<()> {
        let target = normalise(target);
        if target.is_empty() {
            return Err(StorageError::Invalid(
                "alias target must not be blank".into(),
            ));
        }
        let done = sqlx::query("UPDATE aliases SET target = $2 WHERE id = $1")
            .bind(id)
            .bind(&target)
            .execute(&self.pool)
            .await?;
        if done.rows_affected() == 0 {
            return Err(not_found(format!("alias {id}")));
        }
        Ok(())
    }

    /// Delete an alias. Returns `false` when it did not exist.
    pub async fn delete(&self, id: i64) -> Result<bool> {
        let done = sqlx::query("DELETE FROM aliases WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(done.rows_affected() > 0)
    }
}

/// Turn "the `UPDATE` matched no row" into [`StorageError::NotFound`].
fn touched(rows: u64, id: DomainId) -> Result<()> {
    if rows == 0 {
        return Err(not_found(format!("domain {id}")));
    }
    Ok(())
}
