//! `SeaORM` error translation (`REPO-5`).
//!
//! The single place a `sea_orm::DbErr` becomes a domain
//! [`RepositoryError`](budget_domain::RepositoryError). Every repository method
//! routes its driver errors through [`map_db_err`] so the translation rule lives
//! in one location and cannot drift per call-site (`DOMAIN-6`: one shared
//! repository error; `RUST-DOMAIN-4`: framework errors map at the boundary, they
//! do not leak through the public API).
//!
//! Translation table (mirrors the doc on
//! [`budget_domain::RepositoryError`]):
//!   - unique-constraint violation -> [`RepositoryError::UniqueViolation`]
//!   - foreign-key violation -> [`RepositoryError::ForeignKeyViolation`]
//!   - serialization failure (40001) / deadlock (40P01) -> [`RepositoryError::TransactionConflict`]
//!   - record-not-found -> [`RepositoryError::NotFound`]
//!   - everything else -> [`RepositoryError::Database`]

use budget_domain::RepositoryError;
use sea_orm::{DbErr, SqlErr};

/// Translate a `SeaORM` [`DbErr`] into the domain [`RepositoryError`] (`REPO-5`).
///
/// `sql_err()` classifies the SQLSTATE for the unique / foreign-key cases; the
/// serializable-conflict and deadlock SQLSTATEs are matched on the message
/// because `SeaORM` 1.1 does not surface a dedicated variant for them. Anything
/// unclassified falls through to [`RepositoryError::Database`] carrying the
/// original message for diagnosis.
#[must_use]
pub fn map_db_err(err: DbErr) -> RepositoryError {
    match err.sql_err() {
        Some(SqlErr::UniqueConstraintViolation(msg)) => RepositoryError::UniqueViolation(msg),
        Some(SqlErr::ForeignKeyConstraintViolation(msg)) => {
            RepositoryError::ForeignKeyViolation(msg)
        }
        _ => match err {
            DbErr::RecordNotFound(_) => RepositoryError::NotFound,
            other => {
                let msg = other.to_string();
                // Postgres serialization_failure (40001) and deadlock_detected
                // (40P01) are retryable conflicts; SeaORM 1.1 has no dedicated
                // SqlErr variant, so classify on the SQLSTATE text the driver
                // includes in the error message.
                if msg.contains("40001") || msg.contains("40P01") {
                    RepositoryError::TransactionConflict(msg)
                } else {
                    RepositoryError::Database(msg)
                }
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::map_db_err;
    use budget_domain::RepositoryError;
    use sea_orm::DbErr;

    #[test]
    fn record_not_found_maps_to_not_found() {
        let err = DbErr::RecordNotFound("nope".to_string());
        assert_eq!(map_db_err(err), RepositoryError::NotFound);
    }

    #[test]
    fn serialization_failure_sqlstate_maps_to_transaction_conflict() {
        // A non-sql_err DbErr whose message carries the 40001 SQLSTATE must
        // classify as a retryable conflict, not a generic database error.
        let err = DbErr::Custom("could not serialize access (SQLSTATE 40001)".to_string());
        assert!(matches!(
            map_db_err(err),
            RepositoryError::TransactionConflict(_)
        ));
    }

    #[test]
    fn deadlock_sqlstate_maps_to_transaction_conflict() {
        let err = DbErr::Custom("deadlock detected (SQLSTATE 40P01)".to_string());
        assert!(matches!(
            map_db_err(err),
            RepositoryError::TransactionConflict(_)
        ));
    }

    #[test]
    fn unclassified_error_falls_through_to_database() {
        let err = DbErr::Custom("some other failure".to_string());
        assert!(matches!(map_db_err(err), RepositoryError::Database(_)));
    }
}
