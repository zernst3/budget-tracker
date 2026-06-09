//! `SeaORM` [`TransactionRepository`] implementation (`REPO-1`).
//!
//! The widest repository: row CRUD, the rollover/Plaid-dedup/expected lookups,
//! and the two computed read surfaces (per-category spent, month net) that the
//! budget math depends on. Those two are aggregated in a SINGLE SQL query each
//! (`DB-NPLUSONE-1`) and returned as domain projection types
//! ([`CategorySpent`] / [`MonthNet`], `REPO-9`).
//!
//! The status filter on both aggregates encodes
//! `BUDGET-STATUS-DRIVES-INCLUSION-1` exactly: `status IN ('settled',
//! 'expected')` is the SQL form of [`budget_domain::counts_in_budget`] being
//! `true` (pending excluded). The canonical polarity still lives in that
//! predicate; the SQL comment references it so the two cannot silently diverge.

use async_trait::async_trait;
use sea_orm::{
    ColumnTrait, DatabaseBackend, DatabaseConnection, EntityTrait, FromQueryResult, QueryFilter,
    QueryOrder, Statement,
};
use uuid::Uuid;

use budget_domain::RepositoryError;
use budget_domain::ids::{CategoryId, MonthId, TransactionId};
use budget_domain::money::Money;
use budget_domain::projections::{CategorySpent, MonthNet};
use budget_domain::repositories::TransactionRepository;
use budget_domain::transaction::Transaction;
use budget_domain::uow::UnitOfWork;

use budget_entities::transactions;
use budget_mappers::transactions as transactions_mapper;

use crate::conn::with_conn;
use crate::error::map_db_err;
use crate::repositories::map_read;
use crate::upsert::upsert;

/// `SeaORM`-backed [`TransactionRepository`].
pub struct PostgresTransactionRepository {
    db: DatabaseConnection,
}

impl PostgresTransactionRepository {
    /// Build the repository over a connection pool.
    #[must_use]
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }

    /// Map a collection of transaction models into domain transactions.
    fn map_txns(models: Vec<transactions::Model>) -> Result<Vec<Transaction>, RepositoryError> {
        models
            .into_iter()
            .map(|m| transactions_mapper::model_to_domain(m).map_err(map_read))
            .collect()
    }
}

/// Infra-local raw-query row for the per-category spent aggregate
/// (`RUST-SEAORM-RAW-SQL-ESCAPE-1`): this derives `FromQueryResult` so the raw
/// statement parses into a typed shape; it is mapped into the domain
/// [`CategorySpent`] before crossing the trait boundary (the ORM row type never
/// leaks out).
#[derive(Debug, FromQueryResult)]
struct CategorySpentRow {
    category_id: Uuid,
    spent: rust_decimal::Decimal,
}

/// Infra-local raw-query row for the month-net aggregate (see
/// [`CategorySpentRow`] for the pattern rationale).
#[derive(Debug, FromQueryResult)]
struct MonthNetRow {
    net: rust_decimal::Decimal,
}

#[async_trait]
impl TransactionRepository for PostgresTransactionRepository {
    async fn find_by_id(&self, id: TransactionId) -> Result<Option<Transaction>, RepositoryError> {
        let model = transactions::Entity::find_by_id(id.value())
            .one(&self.db)
            .await
            .map_err(map_db_err)?;
        model
            .map(transactions_mapper::model_to_domain)
            .transpose()
            .map_err(map_read)
    }

    async fn list_for_month(&self, month_id: MonthId) -> Result<Vec<Transaction>, RepositoryError> {
        let models = transactions::Entity::find()
            .filter(transactions::Column::MonthId.eq(month_id.value()))
            .order_by_asc(transactions::Column::Date)
            .all(&self.db)
            .await
            .map_err(map_db_err)?;
        Self::map_txns(models)
    }

    async fn list_for_category_in_month(
        &self,
        month_id: MonthId,
        category_id: CategoryId,
    ) -> Result<Vec<Transaction>, RepositoryError> {
        // (month_id, category_id) is backed by ix_transactions_month_category
        // (most-selective column first, SQL-DB-INDEX-2).
        let models = transactions::Entity::find()
            .filter(transactions::Column::MonthId.eq(month_id.value()))
            .filter(transactions::Column::CategoryId.eq(category_id.value()))
            .order_by_asc(transactions::Column::Date)
            .all(&self.db)
            .await
            .map_err(map_db_err)?;
        Self::map_txns(models)
    }

    async fn find_rollover_for_month(
        &self,
        month_id: MonthId,
    ) -> Result<Option<Transaction>, RepositoryError> {
        // The single is_rollover=true row per month, unique by the partial unique
        // index on (month_id) WHERE is_rollover (BUDGET-ROLLOVER-INTEGRITY-1).
        let model = transactions::Entity::find()
            .filter(transactions::Column::MonthId.eq(month_id.value()))
            .filter(transactions::Column::IsRollover.eq(true))
            .one(&self.db)
            .await
            .map_err(map_db_err)?;
        model
            .map(transactions_mapper::model_to_domain)
            .transpose()
            .map_err(map_read)
    }

    async fn find_by_plaid_transaction_id(
        &self,
        plaid_transaction_id: &str,
    ) -> Result<Option<Transaction>, RepositoryError> {
        let model = transactions::Entity::find()
            .filter(transactions::Column::PlaidTransactionId.eq(plaid_transaction_id))
            .one(&self.db)
            .await
            .map_err(map_db_err)?;
        model
            .map(transactions_mapper::model_to_domain)
            .transpose()
            .map_err(map_read)
    }

    async fn list_expected_for_month(
        &self,
        month_id: MonthId,
    ) -> Result<Vec<Transaction>, RepositoryError> {
        let models = transactions::Entity::find()
            .filter(transactions::Column::MonthId.eq(month_id.value()))
            .filter(transactions::Column::Status.eq(transactions::TransactionStatus::Expected))
            .order_by_asc(transactions::Column::Date)
            .all(&self.db)
            .await
            .map_err(map_db_err)?;
        Self::map_txns(models)
    }

    async fn find_expected_matched_to(
        &self,
        real_transaction_id: TransactionId,
    ) -> Result<Option<Transaction>, RepositoryError> {
        // The reverse-path lookup: the single expected placeholder whose
        // matched_transaction_id == the removed real txn (BUDGET-SETTLE-ON-MATCH-1).
        // Backed by ix_transactions_matched_transaction_id (m0003).
        let model = transactions::Entity::find()
            .filter(transactions::Column::MatchedTransactionId.eq(real_transaction_id.value()))
            .one(&self.db)
            .await
            .map_err(map_db_err)?;
        model
            .map(transactions_mapper::model_to_domain)
            .transpose()
            .map_err(map_read)
    }

    async fn category_spent_for_month(
        &self,
        month_id: MonthId,
    ) -> Result<Vec<CategorySpent>, RepositoryError> {
        // Conditional grouped aggregate over transactions; the typed builder
        // cannot express GROUP BY + SUM into a projection cleanly, so use the
        // raw-SQL escape (RUST-SEAORM-RAW-SQL-ESCAPE-1) parsed into
        // CategorySpentRow. Source table: transactions (migration m0001).
        // status IN ('settled','expected') == counts_in_budget()==true
        // (BUDGET-STATUS-DRIVES-INCLUSION-1). category_id IS NOT NULL excludes
        // uncategorized rows (they belong to no bucket).
        // matched_transaction_id IS NULL excludes a matched expected placeholder
        // (BUDGET-SETTLE-ON-MATCH-1): the real txn it links to counts instead, so
        // the pair counts exactly once (BUDGET-NO-DOUBLE-CHARGE-1). Only 'expected'
        // rows ever carry the link, so this never excludes a settled row.
        let sql = "SELECT category_id, SUM(amount) AS spent \
                   FROM transactions \
                   WHERE month_id = $1 \
                     AND category_id IS NOT NULL \
                     AND status IN ('settled', 'expected') \
                     AND matched_transaction_id IS NULL \
                   GROUP BY category_id";
        let stmt = Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            sql,
            [month_id.value().into()],
        );
        let rows = CategorySpentRow::find_by_statement(stmt)
            .all(&self.db)
            .await
            .map_err(map_db_err)?;
        Ok(rows
            .into_iter()
            .map(|r| CategorySpent {
                category_id: CategoryId::new(r.category_id),
                spent: Money::from_decimal(r.spent),
            })
            .collect())
    }

    async fn month_net(&self, month_id: MonthId) -> Result<MonthNet, RepositoryError> {
        // Single scalar aggregate; COALESCE so an empty month nets to 0 rather
        // than NULL (the trait contract returns a zero net, never None). Same
        // inclusion polarity as category_spent_for_month
        // (BUDGET-STATUS-DRIVES-INCLUSION-1). Source: transactions (m0001).
        // matched_transaction_id IS NULL excludes a matched expected placeholder
        // (BUDGET-SETTLE-ON-MATCH-1) so the placeholder/real pair counts once.
        let sql = "SELECT COALESCE(SUM(amount), 0) AS net \
                   FROM transactions \
                   WHERE month_id = $1 \
                     AND status IN ('settled', 'expected') \
                     AND matched_transaction_id IS NULL";
        let stmt = Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            sql,
            [month_id.value().into()],
        );
        let row = MonthNetRow::find_by_statement(stmt)
            .one(&self.db)
            .await
            .map_err(map_db_err)?;
        let net = row.map_or(Money::ZERO, |r| Money::from_decimal(r.net));
        Ok(MonthNet { month_id, net })
    }

    async fn save(
        &self,
        transaction: &Transaction,
        uow: Option<&dyn UnitOfWork>,
    ) -> Result<(), RepositoryError> {
        let active = transactions_mapper::domain_to_active_model(transaction);
        with_conn(&self.db, uow, move |conn| {
            Box::pin(async move { upsert::<transactions::Entity, _, _>(conn, active).await })
        })
        .await
    }

    async fn delete(
        &self,
        id: TransactionId,
        uow: Option<&dyn UnitOfWork>,
    ) -> Result<(), RepositoryError> {
        with_conn(&self.db, uow, move |conn| {
            Box::pin(async move {
                transactions::Entity::delete_by_id(id.value())
                    .exec(conn)
                    .await
                    .map_err(map_db_err)?;
                Ok(())
            })
        })
        .await
    }
}
