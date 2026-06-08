//! Unit-of-work abstractions for cross-aggregate transactions (`REPO-4`,
//! `REPO-6`, `REPO-10`).
//!
//! Several flows in this app are inherently cross-aggregate and must be atomic:
//!   - posting a month's rollover transaction while marking the prior month
//!     closed (`BUDGET-ROLLOVER-INTEGRITY-1`),
//!   - a buffer-financed purchase: insert the transaction, draw the fund, create
//!     the repayment obligation (`SPEC §4.9` D7),
//!   - settling a `flexible_set` placeholder against a real transaction while
//!     updating the category (`BUDGET-SETTLE-ON-MATCH-1`).
//!
//! Per `DOMAIN-1` the domain crate must not import `SeaORM`, so:
//!   - [`UnitOfWork`] is an opaque handle (`as_any` for the infra downcast,
//!     `REPO-6`). Repository write methods take `Option<&dyn UnitOfWork>`:
//!     `None` -> use the pool, `Some` -> enlist in the transaction.
//!   - [`UowProvider`] runs a closure inside a transaction, committing on `Ok`
//!     and rolling back on `Err`. Services hold `Arc<dyn UowProvider>`
//!     (`REPO-10`).
//!
//! The closure signature uses a boxed, pinned future and a boxed return value so
//! the trait is **object-safe** (`Arc<dyn UowProvider>` is the DI shape per
//! `SERVICE-DI-1`). The concrete `SeaOrmUowProvider` lives in
//! `budget-infrastructure`.

use std::any::Any;
use std::future::Future;
use std::pin::Pin;

use async_trait::async_trait;

use crate::error::RepositoryError;

/// An opaque, enlisted transaction handle (`REPO-4` / `REPO-6`).
///
/// The domain only sees `&dyn UnitOfWork`. The infrastructure crate's concrete
/// `SeaOrmUow` (wrapping `Arc<tokio::sync::Mutex<DatabaseTransaction>>`)
/// implements this; repository impls downcast via [`UnitOfWork::as_any`] to
/// reach the real `DatabaseTransaction`. The `Arc<Mutex<..>>` shape (rather than
/// a borrowed `&Tx`) is what makes the handle `'static`, which `dyn Any`
/// requires for the downcast (`REPO-6`).
pub trait UnitOfWork: Send + Sync {
    /// Downcast hook for the infrastructure adapter to recover its concrete
    /// transaction type. Domain code never calls this.
    fn as_any(&self) -> &dyn Any;
}

/// The boxed, pinned future a [`UowProvider`] closure returns.
///
/// Aliased for readability; the boxing is what keeps [`UowProvider`] object-safe
/// so services can hold it as `Arc<dyn UowProvider>` (`REPO-10` / `SERVICE-DI-1`).
pub type UowFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, RepositoryError>> + Send + 'a>>;

/// The boxed closure body run inside a transaction by [`UowProvider::run`].
///
/// It receives the enlisted [`UnitOfWork`] handle and returns a boxed result.
/// Returning `Box<dyn Any + Send>` (instead of a generic `T`) is the technique
/// that keeps the trait object-safe; [`UowProvider::run`] is the generic,
/// type-recovering wrapper callers actually use, and it lives as a default
/// method, not a trait-object method.
type BoxedUowClosure<'a> =
    Box<dyn for<'u> FnOnce(&'u dyn UnitOfWork) -> UowFuture<'u, Box<dyn Any + Send>> + Send + 'a>;

/// Runs a closure inside a database transaction, committing on `Ok` and rolling
/// back on `Err` (`REPO-10`).
///
/// The object-safe core is [`UowProvider::run_boxed`]; the ergonomic, generic
/// [`UowProvider::run`] is a default method that boxes the closure's return
/// value and downcasts it back. Services depend on `Arc<dyn UowProvider>`
/// (`SERVICE-DI-1`); the concrete `SeaOrmUowProvider` lives in
/// `budget-infrastructure`.
#[async_trait]
pub trait UowProvider: Send + Sync {
    /// Object-safe core: run a boxed closure inside a transaction.
    ///
    /// Implementors open a transaction, build the [`UnitOfWork`] handle, invoke
    /// `f`, then commit (on `Ok`) or roll back (on `Err`).
    ///
    /// # Errors
    /// Returns whatever [`RepositoryError`] the closure or the
    /// commit/rollback machinery produces.
    async fn run_boxed(
        &self,
        f: BoxedUowClosure<'_>,
    ) -> Result<Box<dyn Any + Send>, RepositoryError>;

    /// Ergonomic, generic wrapper over [`UowProvider::run_boxed`].
    ///
    /// Boxes the closure's typed result on the way out and downcasts it back on
    /// the way in, so callers write ordinary typed code:
    ///
    /// ```ignore
    /// self.uow.run(|tx| Box::pin(async move {
    ///     self.transactions.insert(&txn, Some(tx)).await?;
    ///     self.funds.draw(fund_id, amount, Some(tx)).await?;
    ///     Ok(())
    /// })).await
    /// ```
    ///
    /// # Errors
    /// Propagates the closure's [`RepositoryError`]. A downcast failure (which
    /// would indicate a logic bug in this wrapper, not a runtime input) is
    /// surfaced as [`RepositoryError::Database`].
    async fn run<F, T>(&self, f: F) -> Result<T, RepositoryError>
    where
        F: for<'u> FnOnce(&'u dyn UnitOfWork) -> UowFuture<'u, T> + Send + 'static,
        T: Send + 'static,
    {
        let boxed: BoxedUowClosure<'static> = Box::new(move |uow| {
            Box::pin(async move {
                let value = f(uow).await?;
                Ok(Box::new(value) as Box<dyn Any + Send>)
            })
        });
        let any = self.run_boxed(boxed).await?;
        any.downcast::<T>().map(|b| *b).map_err(|_| {
            RepositoryError::Database(
                "unit-of-work result downcast failed (internal type mismatch)".to_string(),
            )
        })
    }
}
