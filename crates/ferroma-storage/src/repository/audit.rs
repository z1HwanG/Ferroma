//! `audit_logs` — the admin and security trail.
//!
//! Every privileged action writes one row here: who did it, to what, from where, and
//! whatever detail the caller wants to keep. The table is append-only from this
//! repository's point of view; the only deletion is the retention sweep.

use chrono::{DateTime, Utc};
use sqlx::{PgPool, Postgres, QueryBuilder};

use ferroma_core::UserId;

use crate::error::Result;
use crate::models::AuditLog;
use crate::repository::{limit_of, offset_of};

/// One entry to append to the trail.
#[derive(Debug, Clone)]
pub struct NewAuditLog {
    /// Who acted. `None` for system actions and for failed logins.
    pub actor_user_id: Option<UserId>,
    /// The action, e.g. `user.created`, `domain.deleted`, `login.failed`.
    pub action: String,
    /// The kind of thing acted upon, e.g. `user`, `domain`, `mailbox`.
    pub target_type: Option<String>,
    /// The identifier of that thing, as a string so any id type fits.
    pub target_id: Option<String>,
    /// The peer address the action came from.
    pub ip: Option<String>,
    /// The `User-Agent` that performed it.
    pub user_agent: Option<String>,
    /// Structured detail. Never put secrets in here: the trail is readable by admins.
    pub details: serde_json::Value,
}

/// A filter for reading the trail back.
///
/// Every field is optional and they combine with `AND`.
#[derive(Debug, Clone)]
pub struct AuditFilter {
    /// Only actions by this user.
    pub actor_user_id: Option<UserId>,
    /// Only this action.
    pub action: Option<String>,
    /// Only this target kind.
    pub target_type: Option<String>,
    /// Only entries at or after this instant.
    pub since: Option<DateTime<Utc>>,
    /// Maximum number of rows.
    pub limit: i64,
    /// Rows to skip.
    pub offset: i64,
}

impl AuditFilter {
    /// A filter that matches everything, paged by `limit`/`offset`.
    pub fn new(limit: i64, offset: i64) -> Self {
        AuditFilter {
            actor_user_id: None,
            action: None,
            target_type: None,
            since: None,
            limit,
            offset,
        }
    }
}

/// Admin/security audit trail.
#[derive(Debug, Clone)]
pub struct AuditRepository {
    pool: PgPool,
}

impl AuditRepository {
    /// Build the repository over `pool`.
    pub(crate) fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// The pool this repository queries.
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Append one entry.
    pub async fn record(&self, new: NewAuditLog) -> Result<AuditLog> {
        Ok(sqlx::query_as::<_, AuditLog>(
            "INSERT INTO audit_logs (actor_user_id, action, target_type, target_id, ip,
                                     user_agent, details)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             RETURNING *",
        )
        .bind(new.actor_user_id.map(UserId::get))
        .bind(&new.action)
        .bind(new.target_type.as_deref())
        .bind(new.target_id.as_deref())
        .bind(new.ip.as_deref())
        .bind(new.user_agent.as_deref())
        .bind(&new.details)
        .fetch_one(&self.pool)
        .await?)
    }

    /// Read the trail back, newest first.
    pub async fn list(&self, filter: AuditFilter) -> Result<Vec<AuditLog>> {
        let mut builder = QueryBuilder::<Postgres>::new("SELECT * FROM audit_logs WHERE TRUE");
        push_filter(&mut builder, &filter);
        builder
            .push(" ORDER BY created_at DESC, id DESC LIMIT ")
            .push_bind(limit_of(filter.limit))
            .push(" OFFSET ")
            .push_bind(offset_of(filter.offset));
        Ok(builder
            .build_query_as::<AuditLog>()
            .fetch_all(&self.pool)
            .await?)
    }

    /// How many entries the same filter matches, ignoring `limit`/`offset`.
    pub async fn count(&self, filter: AuditFilter) -> Result<i64> {
        let mut builder = QueryBuilder::<Postgres>::new("SELECT COUNT(*) FROM audit_logs WHERE TRUE");
        push_filter(&mut builder, &filter);
        let (count,): (i64,) = builder.build_query_as().fetch_one(&self.pool).await?;
        Ok(count)
    }

    /// Forget entries older than `cutoff`.
    pub async fn prune_older_than(&self, cutoff: DateTime<Utc>) -> Result<u64> {
        let done = sqlx::query("DELETE FROM audit_logs WHERE created_at < $1")
            .bind(cutoff)
            .execute(&self.pool)
            .await?;
        Ok(done.rows_affected())
    }
}

/// Append the non-empty parts of `filter` to `builder`.
///
/// Every value is bound; nothing from the caller reaches the SQL text.
fn push_filter<'a>(builder: &mut QueryBuilder<'a, Postgres>, filter: &'a AuditFilter) {
    if let Some(actor) = filter.actor_user_id {
        builder.push(" AND actor_user_id = ").push_bind(actor.get());
    }
    if let Some(action) = filter.action.as_deref().filter(|a| !a.is_empty()) {
        builder.push(" AND action = ").push_bind(action);
    }
    if let Some(target_type) = filter.target_type.as_deref().filter(|t| !t.is_empty()) {
        builder
            .push(" AND target_type = ")
            .push_bind(target_type);
    }
    if let Some(since) = filter.since {
        builder.push(" AND created_at >= ").push_bind(since);
    }
}
