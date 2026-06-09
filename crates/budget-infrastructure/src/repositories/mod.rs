//! `SeaORM` repository implementations — one module per aggregate (`REPO-1`).
//!
//! Each `Postgres*Repository` implements the matching domain trait from
//! `budget_domain::repositories`. The boundary rules every module obeys:
//!   - **`REPO-2`**: methods return domain types via the `budget-mappers` crate;
//!     a `SeaORM` `Model` never crosses the trait surface.
//!   - **`REPO-3`**: every method is `async`.
//!   - **`REPO-5`**: driver errors translate through [`crate::error::map_db_err`]
//!     (and read-side mapper failures through [`map_read`]).
//!   - **`REPO-4`/`REPO-6`**: write paths route through
//!     [`crate::conn::with_conn`] so they enlist in a [`SeaOrmUow`] when one is
//!     supplied, else use the pool.
//!   - **`REPO-9`/`REPO-8`**: computed/aggregate reads return domain projection
//!     types ([`budget_domain::projections`]), aggregated in a single SQL query
//!     (`DB-NPLUSONE-1`).
//!
//! `db.*` / `SeaORM` entity access lives ONLY in this crate's repositories — the
//! strict-layering boundary (`ARCH-STRICT-LAYERING-1`): services and domain never
//! touch the database.
//!
//! [`SeaOrmUow`]: crate::uow::SeaOrmUow

pub mod budgets;
pub mod funds;
pub mod months;
pub mod paycheck_config;
pub mod plaid_items;
pub mod transactions;
pub mod users;
pub mod webauthn_credentials;

#[cfg(test)]
mod mock_tests;

use budget_domain::RepositoryError;
use budget_mappers::MapperError;

/// Translate a read-side [`MapperError`] into a [`RepositoryError`] (`REPO-5`).
///
/// A `MapperError` on the read path means a stored value failed a validated
/// newtype constructor — i.e. data corruption (a row that should have been
/// rejected at write time). It surfaces as [`RepositoryError::Database`] carrying
/// the field + reason so the corruption is diagnosable.
// Takes `MapperError` by value because it is used as a `.map_err(map_read)`
// argument, which requires `FnOnce(MapperError)`. The body only formats the
// error; the by-value signature is dictated by the call site, not a real
// consume need.
#[allow(clippy::needless_pass_by_value)]
pub(crate) fn map_read(err: MapperError) -> RepositoryError {
    RepositoryError::Database(format!("stored value failed domain validation: {err}"))
}
