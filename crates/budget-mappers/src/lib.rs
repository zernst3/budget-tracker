//! Translation layer between `SeaORM` Model types and domain types.
//!
//! Sits in its own crate (`MAPPER-1`) so neither `budget-domain` nor
//! `budget-infrastructure` depends on the other. This is the single boundary
//! where the Plaid sign convention is normalized (`BUDGET-PLAID-SIGN-1`: Plaid
//! positive-outflow flips to internal negative-expense exactly once, here).
//!
//! Each public module exposes:
//!   - `model_to_domain(m: Model) -> Result<DomainType, MapperError>` — reading
//!     from the DB. Fallible because validated newtypes (`Email`,
//!     `AccessTokenRef`) can reject a stored value (signalling data corruption).
//!   - `domain_to_active_model(v: &DomainType) -> ActiveModel` — writing to the
//!     DB. Total because every domain value is already valid.
//!
//! Timestamp conversion rule (`DOMAIN-7`): `SeaORM` surfaces `TIMESTAMPTZ` columns
//! as `DateTimeWithTimeZone` (i.e. `chrono::DateTime<chrono::FixedOffset>`).
//! The domain uses `DateTime<Utc>`. Mappers call `.with_timezone(&Utc)` on every
//! timestamp going into the domain, and call `.into()` going back out (chrono
//! implements `From<DateTime<Utc>> for DateTime<FixedOffset>`).
//!
//! Money conversion rule (`BUDGET-MONEY-1` / `DOMAIN-8`): entity `Decimal` fields
//! wrap to `Money::from_decimal` going in, and expose `.as_decimal()` going out.
//! The underlying type is the same `rust_decimal::Decimal`; no precision is lost.
//!
//! ID conversion rule (`DOMAIN-2`): bare `Uuid` fields on the entity side wrap
//! into typed newtype IDs via `From<Uuid>` on the way in, and unwrap via
//! `.value()` on the way out.
//!
//! Plaid sign rule (`BUDGET-PLAID-SIGN-1`): applies only to
//! [`transactions::plaid_model_to_domain`]. Regular [`transactions::model_to_domain`]
//! trusts the stored sign (which has already been normalized). The Plaid-ingest
//! path MUST call [`transactions::plaid_model_to_domain`] which flips the sign
//! once and adds a runtime direction test.

pub mod accounts;
pub mod budgets;
pub mod cash_balances;
pub mod categories;
pub mod funds;
pub mod months;
pub mod paycheck_config;
pub mod plaid_items;
pub mod positions;
pub mod repayment_obligations;
pub mod transactions;
pub mod users;
pub mod webauthn_credentials;

use thiserror::Error;

/// Errors that can arise when translating a stored `Model` into a domain type.
///
/// These indicate data corruption (a stored value fails the validated-newtype
/// constructor that would have caught it at write time — e.g. a blank email in
/// the `users` table). They are unexpected in production; surface them as 500s
/// or log + skip depending on context.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum MapperError {
    /// A stored value failed a validated-newtype constructor (`DOMAIN-3`).
    #[error("field '{field}' stored in the database failed domain validation: {reason}")]
    InvalidStoredValue {
        /// The field whose value was rejected.
        field: &'static str,
        /// The validation error message.
        reason: String,
    },
}

impl From<budget_domain::ValidationError> for MapperError {
    fn from(e: budget_domain::ValidationError) -> Self {
        MapperError::InvalidStoredValue {
            field: "unknown",
            reason: e.to_string(),
        }
    }
}
