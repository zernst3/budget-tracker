//! Domain error types (`DOMAIN-4` / `DOMAIN-6`).
//!
//! Three shared error enums live in the domain crate:
//!   - [`ValidationError`] — returned by `try_new` constructors on validated
//!     newtypes (`DOMAIN-3`): [`crate::validated::Email`], [`crate::money::Money`], etc.
//!   - [`RepositoryError`] — the single shared error type returned by every
//!     repository trait (`DOMAIN-6`). There is exactly ONE of these, not a
//!     per-aggregate variant.
//!   - [`DomainError`] — business-rule violations surfaced by domain logic.
//!
//! Per `DOMAIN-6`, [`RepositoryError`] MUST live here because the repository
//! traits (`REPO-1`) are declared in this crate and reference it in their
//! `Result` types. Higher layers map these into their own typed errors.

use thiserror::Error;

/// Error returned by validated-newtype `try_new` constructors (`DOMAIN-3`).
///
/// Carries the field name and a human-readable reason so callers and tests can
/// match on the failing field without parsing strings.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ValidationError {
    /// A required value was empty / blank.
    #[error("{field} must not be empty")]
    Empty {
        /// The field that failed validation.
        field: &'static str,
    },
    /// A value exceeded its maximum length.
    #[error("{field} must be at most {max} characters (got {actual})")]
    TooLong {
        /// The field that failed validation.
        field: &'static str,
        /// The maximum allowed length.
        max: usize,
        /// The actual length supplied.
        actual: usize,
    },
    /// A value did not match its required format (e.g. an email without `@`).
    #[error("{field} has an invalid format: {reason}")]
    Format {
        /// The field that failed validation.
        field: &'static str,
        /// A human-readable reason.
        reason: String,
    },
    /// A monetary string could not be parsed into an exact decimal.
    #[error("{field} is not a valid monetary amount: {reason}")]
    Money {
        /// The field that failed validation.
        field: &'static str,
        /// A human-readable reason.
        reason: String,
    },
}

/// The single shared repository error (`DOMAIN-6`).
///
/// Every repository trait method returns `Result<_, RepositoryError>`. The
/// concrete `SeaORM` impls in `budget-infrastructure` translate `sea_orm::DbErr`
/// into these variants (unique-violation SQLSTATE -> [`RepositoryError::UniqueViolation`],
/// fk-violation -> [`RepositoryError::ForeignKeyViolation`], serialization
/// failure -> [`RepositoryError::TransactionConflict`], everything else ->
/// [`RepositoryError::Database`]).
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum RepositoryError {
    /// The requested row does not exist.
    #[error("not found")]
    NotFound,
    /// A unique constraint was violated (carries a context string).
    #[error("unique constraint violation: {0}")]
    UniqueViolation(String),
    /// A foreign-key constraint was violated (carries a context string).
    #[error("foreign key violation: {0}")]
    ForeignKeyViolation(String),
    /// A serializable-transaction conflict (caller may retry).
    #[error("transaction conflict: {0}")]
    TransactionConflict(String),
    /// Any other database error (carries the underlying message).
    #[error("database error: {0}")]
    Database(String),
}

/// Business-rule violations surfaced by domain logic (`DOMAIN-4`).
///
/// Distinct from [`RepositoryError`] (a persistence failure) and
/// [`ValidationError`] (a value-construction failure): `DomainError` is for
/// invariants the *business* enforces — e.g. settling an already-settled
/// category, or posting a rollover into a closed month.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum DomainError {
    /// A supplied value failed validation (wraps [`ValidationError`]).
    #[error(transparent)]
    Validation(#[from] ValidationError),
    /// A persistence operation failed (wraps [`RepositoryError`]).
    #[error(transparent)]
    Repository(#[from] RepositoryError),
    /// A business invariant was violated (carries a description).
    #[error("invariant violated: {0}")]
    Invariant(String),
    /// An operation was attempted in an illegal state (carries a description).
    #[error("illegal state: {0}")]
    IllegalState(String),
}
