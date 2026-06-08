//! Executor resolution: pool vs. enlisted transaction (`REPO-4` / `REPO-6`).
//!
//! Every repository method takes the shared [`DatabaseConnection`] (the pool)
//! and, on write paths, an `Option<&dyn UnitOfWork>`. This module collapses that
//! choice into one place: [`with_conn`] runs a closure against an [`Executor`] —
//! a delegating [`ConnectionTrait`] that points at either the enlisted
//! transaction (when a unit of work is supplied) or the pool.
//!
//! [`Executor`] is a small enum that forwards every `ConnectionTrait` method to
//! whichever inner connection it wraps. It exists because `SeaORM` provides no
//! `ConnectionTrait` impl for `&dyn ConnectionTrait` or for a borrowed
//! connection, so a repository write cannot be parameterized over "pool or
//! transaction" without a concrete unifying type. The transaction is borrowed
//! from inside its `tokio::sync::Mutex` guard, so the closure passed to
//! [`with_conn`] keeps that guard's borrow contained (it returns a boxed future
//! tied to the executor's lifetime).

use std::future::Future;
use std::pin::Pin;

use sea_orm::{
    ConnectionTrait, DatabaseConnection, DatabaseTransaction, DbBackend, DbErr, ExecResult,
    QueryResult, Statement,
};

use budget_domain::RepositoryError;
use budget_domain::uow::UnitOfWork;

use crate::uow::SeaOrmUow;

/// A unifying executor over the pool or an enlisted transaction.
///
/// Implements [`ConnectionTrait`] by delegating to the inner connection, so every
/// `SeaORM` query/insert/update call in a repository runs unchanged whether or
/// not it is inside a transaction.
pub(crate) enum Executor<'a> {
    /// The shared connection pool (no unit of work supplied).
    Pool(&'a DatabaseConnection),
    /// An enlisted transaction borrowed from a [`SeaOrmUow`] guard.
    Tx(&'a DatabaseTransaction),
}

#[async_trait::async_trait]
impl ConnectionTrait for Executor<'_> {
    fn get_database_backend(&self) -> DbBackend {
        match self {
            Executor::Pool(c) => c.get_database_backend(),
            Executor::Tx(t) => t.get_database_backend(),
        }
    }

    async fn execute(&self, stmt: Statement) -> Result<ExecResult, DbErr> {
        match self {
            Executor::Pool(c) => c.execute(stmt).await,
            Executor::Tx(t) => t.execute(stmt).await,
        }
    }

    async fn execute_unprepared(&self, sql: &str) -> Result<ExecResult, DbErr> {
        match self {
            Executor::Pool(c) => c.execute_unprepared(sql).await,
            Executor::Tx(t) => t.execute_unprepared(sql).await,
        }
    }

    async fn query_one(&self, stmt: Statement) -> Result<Option<QueryResult>, DbErr> {
        match self {
            Executor::Pool(c) => c.query_one(stmt).await,
            Executor::Tx(t) => t.query_one(stmt).await,
        }
    }

    async fn query_all(&self, stmt: Statement) -> Result<Vec<QueryResult>, DbErr> {
        match self {
            Executor::Pool(c) => c.query_all(stmt).await,
            Executor::Tx(t) => t.query_all(stmt).await,
        }
    }

    fn support_returning(&self) -> bool {
        match self {
            Executor::Pool(c) => c.support_returning(),
            Executor::Tx(t) => t.support_returning(),
        }
    }
}

/// The boxed future a [`with_conn`] closure returns, borrowing the [`Executor`].
pub(crate) type ConnFuture<'e, T> =
    Pin<Box<dyn Future<Output = Result<T, RepositoryError>> + Send + 'e>>;

/// Run `op` against the enlisted transaction if `uow` is `Some`, else the pool.
///
/// The closure receives an [`Executor`]; it is written once, agnostic to whether
/// it runs inside a transaction. When a unit of work is present the transaction's
/// mutex is held for the duration of `op` (a single query), which is correct:
/// writes sharing a transaction are serialized by the service that owns the
/// `UowProvider::run` closure anyway.
///
/// # Errors
/// Propagates the [`RepositoryError`] the closure produces, or a downcast failure
/// from [`SeaOrmUow::downcast`].
pub(crate) async fn with_conn<T, F>(
    pool: &DatabaseConnection,
    uow: Option<&dyn UnitOfWork>,
    op: F,
) -> Result<T, RepositoryError>
where
    F: for<'e> FnOnce(&'e Executor<'e>) -> ConnFuture<'e, T>,
{
    match uow {
        None => {
            let exec = Executor::Pool(pool);
            op(&exec).await
        }
        Some(handle) => {
            let seaorm_uow = SeaOrmUow::downcast(handle)?;
            let guard = seaorm_uow.tx().lock().await;
            let exec = Executor::Tx(&guard);
            op(&exec).await
        }
    }
}
