//! Storage errors.
//!
//! `ferroma-storage` keeps the `sqlx` error type out of its public API: callers get
//! [`StorageError`], which converts into [`ferroma_core::FerromaError`] and preserves
//! the original cause. That keeps `ferroma-core` free of a database dependency while
//! still letting the mail queue decide "retry tomorrow" vs "bounce now" from a
//! single `is_temporary()` check.

use ferroma_core::FerromaError;

/// Anything that can go wrong in the storage layer.
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    /// The database rejected the query, or the connection failed.
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),

    /// Migrations could not be applied.
    #[error("migration error: {0}")]
    Migration(#[from] sqlx::migrate::MigrateError),

    /// A row that the caller required does not exist.
    #[error("not found: {0}")]
    NotFound(String),

    /// A uniqueness or state precondition was violated.
    #[error("conflict: {0}")]
    Conflict(String),

    /// The caller's input cannot be stored (bad flag string, oversized field, …).
    #[error("invalid input: {0}")]
    Invalid(String),

    /// Maildir or attachment filesystem failure.
    #[error("mail store error: {0}")]
    Io(#[from] std::io::Error),

    /// A message's bytes are gone even though its row exists.
    #[error("message body missing at {0}")]
    BodyMissing(String),

    /// The quota check refused the write.
    #[error("quota exceeded for mailbox {mailbox_id}: {used} + {needed} > {limit}")]
    QuotaExceeded {
        /// The mailbox (address) that is full.
        mailbox_id: i64,
        /// Bytes currently stored.
        used: i64,
        /// Bytes the rejected message needs.
        needed: i64,
        /// The configured limit.
        limit: i64,
    },

    /// An `operation_id` was replayed; the cached result is in [`StorageError`]'s caller.
    #[error("duplicate operation: {0}")]
    DuplicateOperation(String),
}

/// Convenience alias.
pub type Result<T, E = StorageError> = std::result::Result<T, E>;

impl StorageError {
    /// Whether retrying the same call later could succeed.
    pub fn is_temporary(&self) -> bool {
        match self {
            StorageError::Database(e) => matches!(
                e,
                sqlx::Error::Io(_)
                    | sqlx::Error::PoolTimedOut
                    | sqlx::Error::PoolClosed
                    | sqlx::Error::Tls(_)
            ),
            StorageError::Migration(_) => false,
            StorageError::NotFound(_) => false,
            StorageError::Conflict(_) => false,
            StorageError::Invalid(_) => false,
            StorageError::Io(_) => true,
            StorageError::BodyMissing(_) => false,
            StorageError::QuotaExceeded { .. } => false,
            StorageError::DuplicateOperation(_) => false,
        }
    }

    /// `true` when PostgreSQL reported a unique-constraint violation (`23505`).
    pub fn is_unique_violation(&self) -> bool {
        as_db_error(self).is_some_and(|e| e.code() == "23505")
    }

    /// `true` when PostgreSQL reported a foreign-key violation (`23503`).
    pub fn is_foreign_key_violation(&self) -> bool {
        as_db_error(self).is_some_and(|e| e.code() == "23503")
    }

    /// `true` when PostgreSQL reported a check-constraint violation (`23514`).
    pub fn is_check_violation(&self) -> bool {
        as_db_error(self).is_some_and(|e| e.code() == "23514")
    }

    /// The name of the constraint that was violated, when PostgreSQL reported one.
    pub fn constraint(&self) -> Option<&str> {
        as_db_error(self).and_then(|e| e.constraint())
    }
}

fn as_db_error(err: &StorageError) -> Option<&sqlx::postgres::PgDatabaseError> {
    match err {
        StorageError::Database(sqlx::Error::Database(db)) => {
            db.try_downcast_ref::<sqlx::postgres::PgDatabaseError>()
        }
        _ => None,
    }
}

impl From<StorageError> for FerromaError {
    fn from(err: StorageError) -> Self {
        match err {
            StorageError::NotFound(what) => FerromaError::NotFound(what),
            StorageError::Conflict(what) => FerromaError::Conflict(what),
            StorageError::Invalid(what) => FerromaError::Invalid(what),
            StorageError::QuotaExceeded {
                mailbox_id,
                used,
                needed,
                limit,
            } => FerromaError::MailboxFull(format!(
                "mailbox {mailbox_id} is full: {used} + {needed} > {limit}"
            )),
            StorageError::DuplicateOperation(op) => {
                FerromaError::Conflict(format!("operation already applied: {op}"))
            }
            StorageError::BodyMissing(path) => {
                FerromaError::NotFound(format!("message body missing at {path}"))
            }
            other => FerromaError::storage(other),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn io_errors_are_temporary_but_validation_is_not() {
        assert!(StorageError::Io(std::io::Error::other("x")).is_temporary());
        assert!(!StorageError::Invalid("x".into()).is_temporary());
        assert!(!StorageError::NotFound("x".into()).is_temporary());
        assert!(!StorageError::DuplicateOperation("op_1".into()).is_temporary());
    }

    #[test]
    fn quota_errors_become_a_temporary_mailbox_full() {
        let err = StorageError::QuotaExceeded {
            mailbox_id: 3,
            used: 1000,
            needed: 100,
            limit: 1050,
        };
        let converted: FerromaError = err.into();
        // Not `LimitExceeded`: a full mailbox must be retryable at the SMTP edge
        // (RFC 3463 `4.2.2`), or the sender bounces mail that would fit tomorrow.
        assert!(matches!(converted, FerromaError::MailboxFull(_)), "{converted:?}");
        assert_eq!(converted.code(), "mailbox_full");
        assert_eq!(converted.http_status(), 413);
        assert!(converted.is_temporary());
    }

    #[test]
    fn not_found_and_conflict_map_onto_their_core_counterparts() {
        let nf: FerromaError = StorageError::NotFound("mailbox 9".into()).into();
        assert!(matches!(nf, FerromaError::NotFound(_)));
        assert_eq!(nf.http_status(), 404);

        let cf: FerromaError = StorageError::Conflict("address taken".into()).into();
        assert!(matches!(cf, FerromaError::Conflict(_)));
        assert_eq!(cf.http_status(), 409);
    }

    #[test]
    fn constraint_helpers_do_not_panic_on_non_database_errors() {
        let err = StorageError::Invalid("x".into());
        assert!(!err.is_unique_violation());
        assert!(!err.is_foreign_key_violation());
        assert!(!err.is_check_violation());
        assert!(err.constraint().is_none());
    }
}
