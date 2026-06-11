//! `SeaORM` [`TransactionRepository`] implementation (`REPO-1`).
//!
//! The widest repository: row CRUD, the rollover/Plaid-dedup/expected lookups,
//! and the per-category spent computed read surface that the budget math depends
//! on. It is aggregated in a SINGLE SQL query (`DB-NPLUSONE-1`) and returned as a
//! domain projection type ([`CategorySpent`], `REPO-9`). (The month-net aggregate
//! was removed: all production net/rollover computation goes through
//! [`budget_app_services::MonthLifecycleService::month_net_for`], the single
//! authoritative path — see `DRIFT_REPORT` MUST-FIX #2 / SHOULD-FIX #5.)
//!
//! The status filter on the aggregate encodes
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
use budget_domain::ids::{CategoryId, MonthId, TransactionId, UserId};
use budget_domain::money::Money;
use budget_domain::projections::CategorySpent;
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

// The per-category spent aggregate SQL (REPO-9, RUST-SEAORM-RAW-SQL-ESCAPE-1).
//
// status IN ('settled','expected') == counts_in_budget()==true
// (BUDGET-STATUS-DRIVES-INCLUSION-1). category_id IS NOT NULL excludes uncategorized
// rows (they belong to no bucket). matched_transaction_id IS NULL excludes a matched
// expected placeholder (BUDGET-SETTLE-ON-MATCH-1): the real txn it links to counts
// instead, so the pair counts exactly once (BUDGET-NO-DOUBLE-CHARGE-1); only
// 'expected' rows carry the link, so this never excludes a settled row.
// is_fund_draw = false excludes a fund DRAW (surplus draw, sinking payout, a
// PayFromSavings triage) — the money was already expensed at contribution time, so
// the categorized draw row must NOT re-charge its category (BUDGET-NO-DOUBLE-CHARGE-1
// / BUDGET-FUND-EARMARK-1); this matches the close-path predicate
// counts_in_month_expense_remaining. The buffer-financed full-price tracking row is
// uncategorized (category_id IS NULL) and is therefore already excluded by
// construction.
//
// is_transfer = false mirrors the `&& !txn.is_transfer` term of
// counts_in_month_expense_remaining (BUDGET-TRANSFER-EXCLUDE-1 / SPEC §4.11 D10): an
// internal account movement (credit-card payment, checking↔savings transfer) is
// EXCLUDED from category spent on BOTH legs (the funding-account outflow AND the
// destination-account inflow), so a transfer can never leak into a category's spent.
// A transfer row may carry a category_id (the user can leave a categorized row that
// they later mark Transfer), so this filter is required here, not implied by
// category_id. Source table: transactions (m0001, columns from m0006).
const CATEGORY_SPENT_SQL: &str = "SELECT category_id, SUM(amount) AS spent \
     FROM transactions \
     WHERE month_id = $1 \
       AND category_id IS NOT NULL \
       AND status IN ('settled', 'expected') \
       AND matched_transaction_id IS NULL \
       AND is_fund_draw = false \
       AND is_transfer = false \
     GROUP BY category_id";

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

    async fn list_pending_inbox(
        &self,
        user_id: UserId,
    ) -> Result<Vec<Transaction>, RepositoryError> {
        // The triage inbox (SPEC §7 / §4.11): settled + uncategorized + NOT a
        // transfer. The `Settled` status filter is what keeps Plaid `pending`
        // charges (status='pending') out of the inbox (SPEC §4.4) — they are
        // excluded by construction, not by a separate guard. The `is_transfer =
        // false` filter (BUDGET-TRANSFER-EXCLUDE-1 / SPEC §4.11 D10) is what lets a
        // triaged transfer LEAVE the inbox without a category: the Transfer treatment
        // sets is_transfer=true (not a category), so the inbox predicate is
        // `status='settled' AND category_id IS NULL AND is_transfer = false`. Scoped
        // to user_id (SPEC §9.1).
        let models = transactions::Entity::find()
            .filter(transactions::Column::UserId.eq(user_id.value()))
            .filter(transactions::Column::Status.eq(transactions::TransactionStatus::Settled))
            .filter(transactions::Column::CategoryId.is_null())
            .filter(transactions::Column::IsTransfer.eq(false))
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
        // Conditional grouped aggregate over transactions; the typed builder cannot
        // express GROUP BY + SUM into a projection cleanly, so use the raw-SQL escape
        // (RUST-SEAORM-RAW-SQL-ESCAPE-1) parsed into CategorySpentRow. The WHERE
        // clause (including the BUDGET-TRANSFER-EXCLUDE-1 `is_transfer = false` mirror)
        // and its rule cross-references are documented on CATEGORY_SPENT_SQL.
        let stmt = Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            CATEGORY_SPENT_SQL,
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

#[cfg(test)]
mod sql_tests {
    //! Structural assertions over the budget-math aggregate SQL
    //! (`ORCH-NEW-PATH-TESTS-1`). `MockDatabase` does not execute the WHERE clause,
    //! so the only honest unit-level guard that the `is_transfer = false` exclusion
    //! is actually in each aggregate is to assert the SQL text carries it. The live
    //! exclusion behaviour is covered by the domain predicate suite and the
    //! `DATABASE_URL`-gated live integration tests.

    use super::CATEGORY_SPENT_SQL;

    #[test]
    fn category_spent_sql_excludes_transfers() {
        // BUDGET-TRANSFER-EXCLUDE-1 / SPEC §4.11 D10: category spent must mirror the
        // predicate's `&& !txn.is_transfer` with `AND is_transfer = false`.
        assert!(
            CATEGORY_SPENT_SQL.contains("is_transfer = false"),
            "category-spent aggregate must exclude transfers (is_transfer = false)"
        );
        // Sibling exclusions still present (regression guard against an edit dropping
        // one while adding the transfer filter).
        assert!(CATEGORY_SPENT_SQL.contains("is_fund_draw = false"));
        assert!(CATEGORY_SPENT_SQL.contains("matched_transaction_id IS NULL"));
        assert!(CATEGORY_SPENT_SQL.contains("status IN ('settled', 'expected')"));
    }
}
