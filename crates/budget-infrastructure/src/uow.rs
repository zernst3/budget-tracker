//! Concrete unit-of-work for cross-aggregate transactions (`REPO-6` / `REPO-10`).
//!
//! The domain declares two ORM-free abstractions ([`budget_domain::UnitOfWork`]
//! and [`budget_domain::UowProvider`]); this module supplies their `SeaORM`
//! implementations:
//!   - [`SeaOrmUow`] wraps the live [`DatabaseTransaction`] behind
//!     `Arc<Mutex<..>>`. The `Arc<Mutex<..>>` shape (rather than a borrowed
//!     `&Tx`) is what makes the handle `'static`, which the `dyn Any` downcast
//!     in [`budget_domain::UnitOfWork::as_any`] requires (`REPO-6`).
//!   - [`SeaOrmUowProvider`] opens a transaction, runs the caller's closure with
//!     the enlisted handle, then commits on `Ok` / rolls back on `Err`
//!     (`REPO-10`).
//!
//! Repository write methods recover the real transaction from an
//! `Option<&dyn UnitOfWork>` via [`SeaOrmUow::downcast`], then resolve an
//! [`Executor`](crate::conn::Executor) that points at either the enlisted
//! transaction or the shared pool.

use std::any::Any;
use std::sync::Arc;

use async_trait::async_trait;
use sea_orm::{DatabaseConnection, DatabaseTransaction, TransactionTrait};
use tokio::sync::Mutex;

use budget_domain::RepositoryError;
use budget_domain::uow::{UnitOfWork, UowFuture, UowProvider};

use crate::error::map_db_err;

/// The boxed closure body the domain's `UowProvider::run_boxed` receives.
///
/// Spelled out here (rather than imported) because the domain crate keeps the
/// alias private; the type must match the trait signature exactly. It is a boxed
/// `FnOnce` taking the enlisted handle and returning a boxed, type-erased future.
type BoxedUowClosure<'a> =
    Box<dyn for<'u> FnOnce(&'u dyn UnitOfWork) -> UowFuture<'u, Box<dyn Any + Send>> + Send + 'a>;

/// The concrete enlisted-transaction handle (`REPO-6`).
///
/// Holds the open [`DatabaseTransaction`] behind `Arc<Mutex<..>>` so the handle
/// is `Clone + 'static` and the repository impls can lock it to run statements
/// inside the transaction. Domain code only ever sees this as
/// `&dyn UnitOfWork`; the [`SeaOrmUow::downcast`] helper recovers the concrete
/// type at the repository boundary.
#[derive(Clone)]
pub struct SeaOrmUow {
    tx: Arc<Mutex<DatabaseTransaction>>,
}

impl SeaOrmUow {
    /// Wrap an open transaction as a unit-of-work handle.
    #[must_use]
    pub fn new(tx: DatabaseTransaction) -> Self {
        Self {
            tx: Arc::new(Mutex::new(tx)),
        }
    }

    /// The shared, lockable transaction this handle wraps.
    pub(crate) fn tx(&self) -> &Arc<Mutex<DatabaseTransaction>> {
        &self.tx
    }

    /// Recover the concrete [`SeaOrmUow`] from a domain handle (`REPO-6`).
    ///
    /// # Errors
    /// [`RepositoryError::Database`] if the handle is not a [`SeaOrmUow`] — that
    /// would mean a foreign `UnitOfWork` implementation was passed, which is a
    /// wiring bug, not a runtime input.
    pub(crate) fn downcast(uow: &dyn UnitOfWork) -> Result<&SeaOrmUow, RepositoryError> {
        uow.as_any().downcast_ref::<SeaOrmUow>().ok_or_else(|| {
            RepositoryError::Database(
                "unit-of-work handle is not a SeaOrmUow (foreign UnitOfWork impl)".to_string(),
            )
        })
    }
}

impl UnitOfWork for SeaOrmUow {
    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// The `SeaORM` [`UowProvider`] — opens a transaction, runs the closure, and
/// commits or rolls back (`REPO-10`).
///
/// Services depend on `Arc<dyn UowProvider>` (`SERVICE-DI-1`); this is the
/// concrete value wired in at the application edge.
pub struct SeaOrmUowProvider {
    db: DatabaseConnection,
}

impl SeaOrmUowProvider {
    /// Build a provider over a connection pool.
    #[must_use]
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }
}

#[async_trait]
impl UowProvider for SeaOrmUowProvider {
    async fn run_boxed(
        &self,
        f: BoxedUowClosure<'_>,
    ) -> Result<Box<dyn Any + Send>, RepositoryError> {
        // Open the transaction, hand an enlisted SeaOrmUow to the closure, and
        // commit on Ok / roll back on Err. The transaction must NOT be locked
        // here while the closure runs — the closure (via repository calls) takes
        // its own lock through SeaOrmUow::tx(); we hand it the Arc-wrapped handle
        // and reclaim ownership for commit/rollback only after the closure
        // completes.
        let tx = self.db.begin().await.map_err(map_db_err)?;
        let uow = SeaOrmUow::new(tx);

        let result: Result<Box<dyn Any + Send>, RepositoryError> = {
            let handle: &dyn UnitOfWork = &uow;
            f(handle).await
        };

        // Reclaim sole ownership of the transaction to commit or roll back. The
        // closure has returned, so no repository call still holds the Arc; the
        // try_unwrap recovers the DatabaseTransaction.
        let tx_arc = uow.tx.clone();
        drop(uow);
        let tx = Arc::try_unwrap(tx_arc)
            .map(Mutex::into_inner)
            .map_err(|_| {
                RepositoryError::Database(
                    "unit-of-work transaction still has outstanding references at \
                     commit time (a repository call retained the handle)"
                        .to_string(),
                )
            })?;

        match result {
            Ok(value) => {
                tx.commit().await.map_err(map_db_err)?;
                Ok(value)
            }
            Err(e) => {
                // Roll back; surface the original closure error, not a rollback
                // error, unless the rollback itself fails catastrophically.
                if let Err(rollback_err) = tx.rollback().await {
                    return Err(RepositoryError::Database(format!(
                        "closure failed ({e}) and rollback also failed ({rollback_err})"
                    )));
                }
                Err(e)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::SeaOrmUow;
    use std::any::Any;

    use budget_domain::RepositoryError;
    use budget_domain::uow::UnitOfWork;

    /// A foreign `UnitOfWork` impl that is NOT a `SeaOrmUow`, to exercise the
    /// downcast-rejection path (`REPO-6`).
    struct ForeignUow;
    impl UnitOfWork for ForeignUow {
        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    #[test]
    fn downcast_rejects_a_foreign_unit_of_work() {
        let foreign = ForeignUow;
        let handle: &dyn UnitOfWork = &foreign;
        let result = SeaOrmUow::downcast(handle);
        assert!(matches!(result, Err(RepositoryError::Database(_))));
    }
}
