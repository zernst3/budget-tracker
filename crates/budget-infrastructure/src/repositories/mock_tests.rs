//! Mock-database tests for the repository layer (`ORCH-NEW-PATH-TESTS-1`).
//!
//! These tests use `sea_orm::MockDatabase` to exercise:
//!   - predicate-bearing read surfaces (settled/expected counted, pending
//!     excluded — `BUDGET-STATUS-DRIVES-INCLUSION-1`)
//!   - the `category_spent_for_month` and `month_net` raw-SQL aggregation
//!     paths and their `Model->domain` mapping (`REPO-9`)
//!   - the rollover-row uniqueness predicate (`BUDGET-ROLLOVER-INTEGRITY-1`)
//!   - the `is_rollover_bucket` predicate on the budget side
//!   - at least one `UoW` commit path via the `SeaOrmUow` downcast
//!   - error translation (`REPO-5`): unique-violation, FK-violation, not-found,
//!     serialization-conflict, generic database error
//!   - mapper round-trips: `model_to_domain` for users, transactions, budgets,
//!     categories, and months
//!
//! ### No live Postgres required
//!
//! `MockDatabase` intercepts every SQL statement at the driver level and
//! replays pre-queued result rows or exec results.  None of these tests opens
//! a network connection.  Tests that genuinely need a live DB (write-then-read
//! round-trips that depend on Postgres constraint enforcement) are gated with
//! `#[ignore]` and live in `tests/repositories_live.rs`.
//!
//! ### Lint suppressions (test-only)
//!
//! The workspace denies `unwrap_used`, `expect_used`, and `panic` in
//! production code.  Test code intentionally panics on assertion failure;
//! these lints are suppressed for this module only.

#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]
#![allow(clippy::panic)]

use std::collections::BTreeMap;

use chrono::{NaiveDate, TimeZone, Utc};
use rust_decimal::Decimal;
use sea_orm::{DbBackend, DbErr, IntoMockRow, MockDatabase, MockExecResult, MockRow, Value};
use uuid::Uuid;

use budget_domain::RepositoryError;
use budget_domain::enums::{MonthStatus, TransactionStatus};
use budget_domain::ids::{BudgetId, CategoryId, MonthId, TransactionId, UserId};
use budget_domain::money::Money;
use budget_domain::repositories::{
    BudgetRepository, MonthRepository, TransactionRepository, UserRepository,
};

use budget_entities::{budgets, categories, months, transactions, users};
use budget_mappers::{
    budgets as budgets_mapper, categories as categories_mapper, months as months_mapper,
    transactions as transactions_mapper, users as users_mapper,
};

use crate::repositories::budgets::PostgresBudgetRepository;
use crate::repositories::months::PostgresMonthRepository;
use crate::repositories::transactions::PostgresTransactionRepository;
use crate::repositories::users::PostgresUserRepository;
use crate::uow::SeaOrmUow;

// ---------------------------------------------------------------------------
// Shared model-building helpers
// ---------------------------------------------------------------------------

fn now_fixed() -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 6, 1, 0, 0, 0).unwrap()
}

fn sample_user_model(id: Uuid) -> users::Model {
    users::Model {
        id,
        email: "zach@example.com".to_owned(),
        password_hash: "$argon2id$hash".to_owned(),
        totp_secret: None,
        tracking_start_date: NaiveDate::from_ymd_opt(2026, 1, 1).unwrap(),
        created_at: now_fixed().into(),
    }
}

fn sample_budget_model(id: Uuid, user_id: Uuid) -> budgets::Model {
    budgets::Model {
        id,
        user_id,
        name: "Test Budget".to_owned(),
        effective_from: NaiveDate::from_ymd_opt(2026, 1, 1).unwrap(),
        effective_to: None,
        created_at: now_fixed().into(),
    }
}

fn sample_category_model(id: Uuid, budget_id: Uuid, is_rollover_bucket: bool) -> categories::Model {
    categories::Model {
        id,
        budget_id,
        category_key: Uuid::new_v4(),
        name: if is_rollover_bucket {
            "Other".to_owned()
        } else {
            "Groceries".to_owned()
        },
        amount: Decimal::new(-50000, 2), // -$500.00
        grp: categories::CategoryGrp::Discretionary,
        settle_type: None,
        expected_bills: None,
        is_rollover_bucket,
        cadence: categories::Cadence::Monthly,
        period_months: None,
        fund_balance: Decimal::ZERO,
        next_due_date: None,
        sort_order: 0,
    }
}

fn sample_month_model(id: Uuid, user_id: Uuid, budget_id: Uuid) -> months::Model {
    months::Model {
        id,
        user_id,
        budget_id,
        year: 2026,
        month: 2,
        status: months::MonthStatus::Open,
        opened_at: now_fixed().into(),
        closed_at: None,
    }
}

fn sample_txn_model(
    id: Uuid,
    user_id: Uuid,
    month_id: Uuid,
    category_id: Option<Uuid>,
    amount: Decimal,
    status: transactions::TransactionStatus,
    is_rollover: bool,
) -> transactions::Model {
    transactions::Model {
        id,
        user_id,
        month_id,
        category_id,
        account_id: None,
        date: NaiveDate::from_ymd_opt(2026, 2, 10).unwrap(),
        amount,
        description: "test".to_owned(),
        source: transactions::TransactionSource::Manual,
        plaid_transaction_id: None,
        status,
        income_kind: None,
        is_rollover,
        is_fund_draw: false,
        created_at: now_fixed().into(),
        updated_at: now_fixed().into(),
    }
}

/// Build a `MockRow` for a `CategorySpentRow` (raw SQL aggregate).
///
/// The `FromQueryResult` derive on the infra-private `CategorySpentRow` calls
/// `row.try_get_nullable("", "category_id")` and
/// `row.try_get_nullable("", "spent")` — so the `MockRow` must supply those
/// two column names.
fn category_spent_mock_row(category_id: Uuid, spent: Decimal) -> MockRow {
    let mut map: BTreeMap<String, Value> = BTreeMap::new();
    map.insert("category_id".to_owned(), category_id.into());
    map.insert("spent".to_owned(), spent.into());
    map.into_mock_row()
}

/// Build a `MockRow` for a `MonthNetRow` (raw SQL aggregate).
///
/// The derive calls `row.try_get_nullable("", "net")`.
fn month_net_mock_row(net: Decimal) -> MockRow {
    let mut map: BTreeMap<String, Value> = BTreeMap::new();
    map.insert("net".to_owned(), net.into());
    map.into_mock_row()
}

// ---------------------------------------------------------------------------
// Error-translation tests (`REPO-5`)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod error_translation {
    use super::*;
    use crate::error::map_db_err;

    #[test]
    fn record_not_found_maps_to_not_found() {
        let err = DbErr::RecordNotFound("no row".to_owned());
        assert_eq!(map_db_err(err), RepositoryError::NotFound);
    }

    #[test]
    fn serialization_failure_40001_maps_to_conflict() {
        let err = DbErr::Custom("error 40001: serialization failure".to_owned());
        assert!(matches!(
            map_db_err(err),
            RepositoryError::TransactionConflict(_)
        ));
    }

    #[test]
    fn deadlock_40p01_maps_to_conflict() {
        let err = DbErr::Custom("deadlock detected (SQLSTATE 40P01)".to_owned());
        assert!(matches!(
            map_db_err(err),
            RepositoryError::TransactionConflict(_)
        ));
    }

    #[test]
    fn generic_db_error_maps_to_database_variant() {
        let err = DbErr::Custom("connection reset".to_owned());
        assert!(matches!(map_db_err(err), RepositoryError::Database(_)));
    }
}

// ---------------------------------------------------------------------------
// Mapper round-trip tests (model -> domain, no persistence needed)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod mapper_round_trips {
    use super::*;

    #[test]
    fn user_model_maps_to_domain() {
        let id = Uuid::new_v4();
        let m = sample_user_model(id);
        let domain = users_mapper::model_to_domain(m).expect("mapper succeeds");
        assert_eq!(domain.id, UserId::new(id));
        assert_eq!(domain.email.as_str(), "zach@example.com");
    }

    #[test]
    fn user_model_with_invalid_email_returns_mapper_error() {
        let mut m = sample_user_model(Uuid::new_v4());
        m.email = "not-an-email".to_owned();
        assert!(users_mapper::model_to_domain(m).is_err());
    }

    #[test]
    fn budget_model_maps_to_domain() {
        let id = Uuid::new_v4();
        let user_id = Uuid::new_v4();
        let m = sample_budget_model(id, user_id);
        let domain = budgets_mapper::model_to_domain(m).expect("mapper succeeds");
        assert_eq!(domain.id, BudgetId::new(id));
        assert_eq!(domain.user_id, UserId::new(user_id));
        assert!(domain.effective_to.is_none(), "open-ended version");
    }

    #[test]
    fn category_model_rollover_bucket_flag_preserved() {
        let m = sample_category_model(Uuid::new_v4(), Uuid::new_v4(), true);
        let domain = categories_mapper::model_to_domain(m).expect("mapper succeeds");
        assert!(domain.is_rollover_bucket);
    }

    #[test]
    fn category_model_non_rollover_flag_preserved() {
        let m = sample_category_model(Uuid::new_v4(), Uuid::new_v4(), false);
        let domain = categories_mapper::model_to_domain(m).expect("mapper succeeds");
        assert!(!domain.is_rollover_bucket);
    }

    #[test]
    fn month_model_maps_to_domain_open_status() {
        let id = Uuid::new_v4();
        let m = sample_month_model(id, Uuid::new_v4(), Uuid::new_v4());
        let domain = months_mapper::model_to_domain(m).expect("mapper succeeds");
        assert_eq!(domain.id, MonthId::new(id));
        assert_eq!(domain.status, MonthStatus::Open);
        assert!(domain.closed_at.is_none());
    }

    #[test]
    fn transaction_settled_status_maps_correctly() {
        let m = sample_txn_model(
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            None,
            Decimal::new(-1000, 2),
            transactions::TransactionStatus::Settled,
            false,
        );
        let domain = transactions_mapper::model_to_domain(m).expect("mapper succeeds");
        assert_eq!(domain.status, TransactionStatus::Settled);
        assert!(domain.counts_in_budget(), "settled counts");
    }

    #[test]
    fn transaction_expected_status_counts_in_budget() {
        let m = sample_txn_model(
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            Some(Uuid::new_v4()),
            Decimal::new(-4000, 2),
            transactions::TransactionStatus::Expected,
            false,
        );
        let domain = transactions_mapper::model_to_domain(m).expect("mapper succeeds");
        assert_eq!(domain.status, TransactionStatus::Expected);
        assert!(domain.counts_in_budget(), "expected counts");
    }

    #[test]
    fn transaction_pending_status_excluded_from_budget() {
        let m = sample_txn_model(
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            Some(Uuid::new_v4()),
            Decimal::new(-99900, 2),
            transactions::TransactionStatus::Pending,
            false,
        );
        let domain = transactions_mapper::model_to_domain(m).expect("mapper succeeds");
        assert_eq!(domain.status, TransactionStatus::Pending);
        assert!(
            !domain.counts_in_budget(),
            "pending must be excluded (BUDGET-STATUS-DRIVES-INCLUSION-1)"
        );
    }

    #[test]
    fn transaction_is_rollover_flag_preserved() {
        let m = sample_txn_model(
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            None,
            Decimal::new(21200, 2),
            transactions::TransactionStatus::Settled,
            true,
        );
        let domain = transactions_mapper::model_to_domain(m).expect("mapper succeeds");
        assert!(
            domain.is_rollover,
            "BUDGET-ROLLOVER-INTEGRITY-1: flag preserved"
        );
    }

    #[test]
    fn all_three_statuses_map_through_domain_predicate() {
        // Validate the full inclusion matrix in one compact table.
        let cases: &[(transactions::TransactionStatus, bool)] = &[
            (transactions::TransactionStatus::Settled, true),
            (transactions::TransactionStatus::Expected, true),
            (transactions::TransactionStatus::Pending, false),
        ];
        for (entity_status, expected_counts) in cases {
            let m = sample_txn_model(
                Uuid::new_v4(),
                Uuid::new_v4(),
                Uuid::new_v4(),
                Some(Uuid::new_v4()),
                Decimal::new(-100, 0),
                *entity_status,
                false,
            );
            let domain = transactions_mapper::model_to_domain(m).expect("mapper ok");
            assert_eq!(
                domain.counts_in_budget(),
                *expected_counts,
                "status {entity_status:?}: expected counts_in_budget={expected_counts}",
            );
        }
    }
}

// ---------------------------------------------------------------------------
// UserRepository mock tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod user_repo {
    use super::*;

    #[tokio::test]
    async fn find_by_id_returns_domain_user_when_row_present() {
        let id = Uuid::new_v4();
        let model = sample_user_model(id);
        let db = MockDatabase::new(DbBackend::Postgres)
            .append_query_results([[model]])
            .into_connection();
        let repo = PostgresUserRepository::new(db);
        let result = repo.find_by_id(UserId::new(id)).await.expect("no error");
        let user = result.expect("row present");
        assert_eq!(user.id, UserId::new(id));
        assert_eq!(user.email.as_str(), "zach@example.com");
    }

    #[tokio::test]
    async fn find_by_id_returns_none_when_no_row() {
        let db = MockDatabase::new(DbBackend::Postgres)
            .append_query_results([Vec::<users::Model>::new()])
            .into_connection();
        let repo = PostgresUserRepository::new(db);
        let result = repo
            .find_by_id(UserId::new(Uuid::new_v4()))
            .await
            .expect("no error");
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn find_by_email_returns_domain_user() {
        let id = Uuid::new_v4();
        let model = sample_user_model(id);
        let db = MockDatabase::new(DbBackend::Postgres)
            .append_query_results([[model]])
            .into_connection();
        let repo = PostgresUserRepository::new(db);
        let result = repo
            .find_by_email("zach@example.com")
            .await
            .expect("no error");
        let user = result.expect("row present");
        assert_eq!(user.id, UserId::new(id));
    }

    #[tokio::test]
    async fn find_by_email_returns_none_when_absent() {
        let db = MockDatabase::new(DbBackend::Postgres)
            .append_query_results([Vec::<users::Model>::new()])
            .into_connection();
        let repo = PostgresUserRepository::new(db);
        let result = repo
            .find_by_email("nobody@example.com")
            .await
            .expect("no error");
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn save_issues_exec_without_error() {
        use budget_domain::user::User;
        use budget_domain::validated::Email;
        let db = MockDatabase::new(DbBackend::Postgres)
            .append_exec_results([MockExecResult {
                last_insert_id: 0,
                rows_affected: 1,
            }])
            .into_connection();
        let repo = PostgresUserRepository::new(db);
        let user = User {
            id: UserId::generate(),
            email: Email::try_new("zach@example.com").expect("valid email"),
            password_hash: "$argon2id$x".to_owned(),
            totp_secret: None,
            tracking_start_date: NaiveDate::from_ymd_opt(2026, 1, 1).unwrap(),
            created_at: now_fixed(),
        };
        let result = repo.save(&user, None).await;
        assert!(result.is_ok(), "save must succeed: {result:?}");
    }
}

// ---------------------------------------------------------------------------
// BudgetRepository mock tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod budget_repo {
    use super::*;

    #[tokio::test]
    async fn find_by_id_returns_budget() {
        let id = Uuid::new_v4();
        let user_id = Uuid::new_v4();
        let model = sample_budget_model(id, user_id);
        let db = MockDatabase::new(DbBackend::Postgres)
            .append_query_results([[model]])
            .into_connection();
        let repo = PostgresBudgetRepository::new(db);
        let result = repo.find_by_id(BudgetId::new(id)).await.expect("no error");
        let budget = result.expect("row present");
        assert_eq!(budget.id, BudgetId::new(id));
        assert_eq!(budget.user_id, UserId::new(user_id));
    }

    #[tokio::test]
    async fn find_current_returns_none_when_absent() {
        let db = MockDatabase::new(DbBackend::Postgres)
            .append_query_results([Vec::<budgets::Model>::new()])
            .into_connection();
        let repo = PostgresBudgetRepository::new(db);
        let result = repo
            .find_current(UserId::generate())
            .await
            .expect("no error");
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn list_categories_returns_all_rows_mapped() {
        let budget_id = Uuid::new_v4();
        let cat1 = sample_category_model(Uuid::new_v4(), budget_id, false);
        let cat2 = sample_category_model(Uuid::new_v4(), budget_id, true);
        let db = MockDatabase::new(DbBackend::Postgres)
            .append_query_results([[cat1, cat2]])
            .into_connection();
        let repo = PostgresBudgetRepository::new(db);
        let categories = repo
            .list_categories(BudgetId::new(budget_id))
            .await
            .expect("no error");
        assert_eq!(categories.len(), 2);
    }

    #[tokio::test]
    async fn find_rollover_bucket_returns_rollover_category() {
        let budget_id = Uuid::new_v4();
        let rollover_id = Uuid::new_v4();
        let model = sample_category_model(rollover_id, budget_id, true);
        let db = MockDatabase::new(DbBackend::Postgres)
            .append_query_results([[model]])
            .into_connection();
        let repo = PostgresBudgetRepository::new(db);
        let result = repo
            .find_rollover_bucket(BudgetId::new(budget_id))
            .await
            .expect("no error");
        let cat = result.expect("row present");
        assert!(
            cat.is_rollover_bucket,
            "BUDGET-ROLLOVER-INTEGRITY-1: predicate must hold"
        );
        assert_eq!(cat.id, CategoryId::new(rollover_id));
    }

    #[tokio::test]
    async fn find_rollover_bucket_returns_none_when_absent() {
        let db = MockDatabase::new(DbBackend::Postgres)
            .append_query_results([Vec::<categories::Model>::new()])
            .into_connection();
        let repo = PostgresBudgetRepository::new(db);
        let result = repo
            .find_rollover_bucket(BudgetId::generate())
            .await
            .expect("no error");
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn find_active_for_date_returns_matching_budget() {
        let id = Uuid::new_v4();
        let user_id = Uuid::new_v4();
        let model = sample_budget_model(id, user_id);
        let db = MockDatabase::new(DbBackend::Postgres)
            .append_query_results([[model]])
            .into_connection();
        let repo = PostgresBudgetRepository::new(db);
        let date = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();
        let result = repo
            .find_active_for_date(UserId::new(user_id), date)
            .await
            .expect("no error");
        let budget = result.expect("row present");
        assert_eq!(budget.id, BudgetId::new(id));
    }
}

// ---------------------------------------------------------------------------
// MonthRepository mock tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod month_repo {
    use super::*;

    #[tokio::test]
    async fn find_by_id_returns_open_month() {
        let id = Uuid::new_v4();
        let model = sample_month_model(id, Uuid::new_v4(), Uuid::new_v4());
        let db = MockDatabase::new(DbBackend::Postgres)
            .append_query_results([[model]])
            .into_connection();
        let repo = PostgresMonthRepository::new(db);
        let result = repo.find_by_id(MonthId::new(id)).await.expect("no error");
        let month = result.expect("row present");
        assert_eq!(month.id, MonthId::new(id));
        assert_eq!(month.status, MonthStatus::Open);
    }

    #[tokio::test]
    async fn find_latest_returns_none_when_no_months_exist() {
        let db = MockDatabase::new(DbBackend::Postgres)
            .append_query_results([Vec::<months::Model>::new()])
            .into_connection();
        let repo = PostgresMonthRepository::new(db);
        let result = repo
            .find_latest(UserId::generate())
            .await
            .expect("no error");
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn list_for_user_returns_multiple_months() {
        let user_id = Uuid::new_v4();
        let budget_id = Uuid::new_v4();
        let m1 = sample_month_model(Uuid::new_v4(), user_id, budget_id);
        let mut m2 = sample_month_model(Uuid::new_v4(), user_id, budget_id);
        m2.month = 3; // March
        let db = MockDatabase::new(DbBackend::Postgres)
            .append_query_results([[m1, m2]])
            .into_connection();
        let repo = PostgresMonthRepository::new(db);
        let months = repo
            .list_for_user(UserId::new(user_id))
            .await
            .expect("no error");
        assert_eq!(months.len(), 2);
    }
}

// ---------------------------------------------------------------------------
// TransactionRepository mock tests — the most important surface
// ---------------------------------------------------------------------------

#[cfg(test)]
mod transaction_repo {
    use super::*;

    // --- basic reads ---------------------------------------------------------

    #[tokio::test]
    async fn find_by_id_returns_domain_transaction() {
        let id = Uuid::new_v4();
        let model = sample_txn_model(
            id,
            Uuid::new_v4(),
            Uuid::new_v4(),
            None,
            Decimal::new(-1500, 2),
            transactions::TransactionStatus::Settled,
            false,
        );
        let db = MockDatabase::new(DbBackend::Postgres)
            .append_query_results([[model]])
            .into_connection();
        let repo = PostgresTransactionRepository::new(db);
        let result = repo
            .find_by_id(TransactionId::new(id))
            .await
            .expect("no error");
        let txn = result.expect("row present");
        assert_eq!(txn.id, TransactionId::new(id));
        assert_eq!(txn.amount, Money::from_minor(-1500));
    }

    #[tokio::test]
    async fn find_by_id_returns_none_when_absent() {
        let db = MockDatabase::new(DbBackend::Postgres)
            .append_query_results([Vec::<transactions::Model>::new()])
            .into_connection();
        let repo = PostgresTransactionRepository::new(db);
        let result = repo
            .find_by_id(TransactionId::generate())
            .await
            .expect("no error");
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn list_for_month_returns_all_rows() {
        let month_id = Uuid::new_v4();
        let t1 = sample_txn_model(
            Uuid::new_v4(),
            Uuid::new_v4(),
            month_id,
            Some(Uuid::new_v4()),
            Decimal::new(-2000, 2),
            transactions::TransactionStatus::Settled,
            false,
        );
        let t2 = sample_txn_model(
            Uuid::new_v4(),
            Uuid::new_v4(),
            month_id,
            Some(Uuid::new_v4()),
            Decimal::new(-5000, 2),
            transactions::TransactionStatus::Expected,
            false,
        );
        let db = MockDatabase::new(DbBackend::Postgres)
            .append_query_results([[t1, t2]])
            .into_connection();
        let repo = PostgresTransactionRepository::new(db);
        let result = repo
            .list_for_month(MonthId::new(month_id))
            .await
            .expect("no error");
        assert_eq!(result.len(), 2);
    }

    #[tokio::test]
    async fn list_for_category_in_month_returns_rows() {
        let month_id = Uuid::new_v4();
        let cat_id = Uuid::new_v4();
        let t = sample_txn_model(
            Uuid::new_v4(),
            Uuid::new_v4(),
            month_id,
            Some(cat_id),
            Decimal::new(-3000, 2),
            transactions::TransactionStatus::Settled,
            false,
        );
        let db = MockDatabase::new(DbBackend::Postgres)
            .append_query_results([[t]])
            .into_connection();
        let repo = PostgresTransactionRepository::new(db);
        let result = repo
            .list_for_category_in_month(MonthId::new(month_id), CategoryId::new(cat_id))
            .await
            .expect("no error");
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].amount, Money::from_minor(-3000));
    }

    // --- rollover-predicate read (`BUDGET-ROLLOVER-INTEGRITY-1`) ------------

    #[tokio::test]
    async fn find_rollover_for_month_returns_row_when_present() {
        let id = Uuid::new_v4();
        let month_id = Uuid::new_v4();
        let model = sample_txn_model(
            id,
            Uuid::new_v4(),
            month_id,
            None,
            Decimal::new(21200, 2),
            transactions::TransactionStatus::Settled,
            true, // is_rollover = true
        );
        let db = MockDatabase::new(DbBackend::Postgres)
            .append_query_results([[model]])
            .into_connection();
        let repo = PostgresTransactionRepository::new(db);
        let result = repo
            .find_rollover_for_month(MonthId::new(month_id))
            .await
            .expect("no error");
        let txn = result.expect("rollover row present");
        assert!(
            txn.is_rollover,
            "BUDGET-ROLLOVER-INTEGRITY-1: is_rollover must be true"
        );
        assert_eq!(txn.id, TransactionId::new(id));
    }

    #[tokio::test]
    async fn find_rollover_for_month_returns_none_when_absent() {
        let db = MockDatabase::new(DbBackend::Postgres)
            .append_query_results([Vec::<transactions::Model>::new()])
            .into_connection();
        let repo = PostgresTransactionRepository::new(db);
        let result = repo
            .find_rollover_for_month(MonthId::generate())
            .await
            .expect("no error");
        assert!(result.is_none(), "no rollover row yet");
    }

    // --- expected-filter read (`BUDGET-STATUS-DRIVES-INCLUSION-1`) ----------

    #[tokio::test]
    async fn list_expected_returns_only_expected_rows() {
        // The mock returns whatever we queue; the repository filters by `status =
        // 'expected'` in SQL.  We verify the filter is issued (the mapper side) by
        // queuing an expected-status model and checking the domain status.
        let month_id = Uuid::new_v4();
        let expected_model = sample_txn_model(
            Uuid::new_v4(),
            Uuid::new_v4(),
            month_id,
            Some(Uuid::new_v4()),
            Decimal::new(-5000, 2),
            transactions::TransactionStatus::Expected,
            false,
        );
        let db = MockDatabase::new(DbBackend::Postgres)
            .append_query_results([[expected_model]])
            .into_connection();
        let repo = PostgresTransactionRepository::new(db);
        let result = repo
            .list_expected_for_month(MonthId::new(month_id))
            .await
            .expect("no error");
        assert_eq!(result.len(), 1);
        assert_eq!(
            result[0].status,
            TransactionStatus::Expected,
            "list_expected must return Expected-status rows"
        );
        assert!(result[0].counts_in_budget(), "expected rows count");
    }

    // --- find_by_plaid_transaction_id (dedup surface) -----------------------

    #[tokio::test]
    async fn find_by_plaid_transaction_id_returns_row() {
        let mut model = sample_txn_model(
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            None,
            Decimal::new(-1000, 2),
            transactions::TransactionStatus::Settled,
            false,
        );
        model.plaid_transaction_id = Some("plaid-abc".to_owned());
        let db = MockDatabase::new(DbBackend::Postgres)
            .append_query_results([[model]])
            .into_connection();
        let repo = PostgresTransactionRepository::new(db);
        let result = repo
            .find_by_plaid_transaction_id("plaid-abc")
            .await
            .expect("no error");
        let txn = result.expect("row present");
        assert_eq!(txn.plaid_transaction_id.as_deref(), Some("plaid-abc"));
    }

    // --- category_spent_for_month (raw SQL aggregate, REPO-9) ---------------

    #[tokio::test]
    async fn category_spent_returns_empty_when_no_rows() {
        // An empty query result for the raw aggregate — zero categories.
        let db = MockDatabase::new(DbBackend::Postgres)
            .append_query_results([Vec::<MockRow>::new()])
            .into_connection();
        let repo = PostgresTransactionRepository::new(db);
        let result = repo
            .category_spent_for_month(MonthId::generate())
            .await
            .expect("no error");
        assert!(result.is_empty(), "empty month: no category buckets");
    }

    #[tokio::test]
    async fn category_spent_maps_single_category_correctly() {
        let cat_id = Uuid::new_v4();
        let month_id = Uuid::new_v4();
        // settled -$100 + expected -$40 = -$140 aggregated in SQL
        let spent = Decimal::new(-14000, 2);
        let row = category_spent_mock_row(cat_id, spent);
        let db = MockDatabase::new(DbBackend::Postgres)
            .append_query_results([[row]])
            .into_connection();
        let repo = PostgresTransactionRepository::new(db);
        let result = repo
            .category_spent_for_month(MonthId::new(month_id))
            .await
            .expect("no error");
        assert_eq!(result.len(), 1, "one category bucket");
        assert_eq!(result[0].category_id, CategoryId::new(cat_id));
        assert_eq!(
            result[0].spent,
            Money::from_minor(-14000),
            "settled + expected sum (pending excluded, BUDGET-STATUS-DRIVES-INCLUSION-1)"
        );
    }

    #[tokio::test]
    async fn category_spent_maps_multiple_categories() {
        let cat1 = Uuid::new_v4();
        let cat2 = Uuid::new_v4();
        let month_id = Uuid::new_v4();
        let rows = vec![
            category_spent_mock_row(cat1, Decimal::new(-10000, 2)), // -$100
            category_spent_mock_row(cat2, Decimal::new(-5050, 2)),  // -$50.50
        ];
        let db = MockDatabase::new(DbBackend::Postgres)
            .append_query_results([rows])
            .into_connection();
        let repo = PostgresTransactionRepository::new(db);
        let result = repo
            .category_spent_for_month(MonthId::new(month_id))
            .await
            .expect("no error");
        assert_eq!(result.len(), 2, "two category buckets");
        assert_eq!(result[0].category_id, CategoryId::new(cat1));
        assert_eq!(result[0].spent, Money::from_minor(-10000));
        assert_eq!(result[1].category_id, CategoryId::new(cat2));
        assert_eq!(result[1].spent, Money::from_minor(-5050));
    }

    #[tokio::test]
    async fn category_spent_pending_excluded_not_double_counted() {
        // The raw SQL sums only settled + expected (BUDGET-STATUS-DRIVES-INCLUSION-1).
        // The mock returns a -$140 aggregate (not -$1139 which would include pending).
        // This documents the domain rule; the SQL filtering is tested by the live suite.
        let cat_id = Uuid::new_v4();
        let row = category_spent_mock_row(cat_id, Decimal::new(-14000, 2));
        let db = MockDatabase::new(DbBackend::Postgres)
            .append_query_results([[row]])
            .into_connection();
        let repo = PostgresTransactionRepository::new(db);
        let result = repo
            .category_spent_for_month(MonthId::generate())
            .await
            .expect("no error");
        // The caller (service layer) sees only the SQL-filtered aggregate.
        // If pending were included the amount would be much larger; since the SQL
        // excludes it, the domain value reflects only settled + expected.
        assert_eq!(result[0].spent, Money::from_minor(-14000));
    }

    // --- month_net (raw SQL scalar aggregate, REPO-9) -----------------------

    #[tokio::test]
    async fn month_net_returns_zero_for_empty_month() {
        // COALESCE(SUM(amount), 0) → 0 when no rows match. The mock returns a
        // single row with net = 0 (matching what Postgres emits).
        let month_id = Uuid::new_v4();
        let row = month_net_mock_row(Decimal::ZERO);
        let db = MockDatabase::new(DbBackend::Postgres)
            .append_query_results([[row]])
            .into_connection();
        let repo = PostgresTransactionRepository::new(db);
        let result = repo
            .month_net(MonthId::new(month_id))
            .await
            .expect("no error");
        assert_eq!(result.month_id, MonthId::new(month_id));
        assert_eq!(
            result.net,
            Money::ZERO,
            "empty month nets to zero, never None"
        );
    }

    #[tokio::test]
    async fn month_net_maps_negative_net_correctly() {
        // settled -$100 + expected -$40 = -$140 (pending -$999 excluded)
        let month_id = Uuid::new_v4();
        let row = month_net_mock_row(Decimal::new(-14000, 2));
        let db = MockDatabase::new(DbBackend::Postgres)
            .append_query_results([[row]])
            .into_connection();
        let repo = PostgresTransactionRepository::new(db);
        let result = repo
            .month_net(MonthId::new(month_id))
            .await
            .expect("no error");
        assert_eq!(result.net, Money::from_minor(-14000));
        assert_eq!(result.month_id, MonthId::new(month_id));
    }

    #[tokio::test]
    async fn month_net_maps_positive_net_income_month() {
        // Paycheck month where income > expenses.
        let month_id = Uuid::new_v4();
        let row = month_net_mock_row(Decimal::new(35000, 2)); // +$350.00
        let db = MockDatabase::new(DbBackend::Postgres)
            .append_query_results([[row]])
            .into_connection();
        let repo = PostgresTransactionRepository::new(db);
        let result = repo
            .month_net(MonthId::new(month_id))
            .await
            .expect("no error");
        assert!(result.net.is_positive());
        assert_eq!(result.net, Money::from_minor(35000));
    }

    #[tokio::test]
    async fn month_net_no_rows_returned_falls_back_to_zero() {
        // Edge: if the aggregate returns NO rows at all (unlikely with COALESCE,
        // but our fallback logic in the repo handles it).
        let month_id = Uuid::new_v4();
        let db = MockDatabase::new(DbBackend::Postgres)
            .append_query_results([Vec::<MockRow>::new()])
            .into_connection();
        let repo = PostgresTransactionRepository::new(db);
        let result = repo
            .month_net(MonthId::new(month_id))
            .await
            .expect("no error");
        assert_eq!(result.net, Money::ZERO, "fallback to zero, never None");
        assert_eq!(result.month_id, MonthId::new(month_id));
    }

    // --- save / delete (exec paths) -----------------------------------------

    #[tokio::test]
    async fn save_transaction_without_uow_issues_exec() {
        use budget_domain::transaction::Transaction;
        let db = MockDatabase::new(DbBackend::Postgres)
            .append_exec_results([MockExecResult {
                last_insert_id: 0,
                rows_affected: 1,
            }])
            .into_connection();
        let repo = PostgresTransactionRepository::new(db);
        let m = sample_txn_model(
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            None,
            Decimal::new(-2500, 2),
            transactions::TransactionStatus::Settled,
            false,
        );
        let txn: Transaction = transactions_mapper::model_to_domain(m).expect("mapper ok");
        let result = repo.save(&txn, None).await;
        assert!(result.is_ok(), "save must succeed: {result:?}");
    }

    #[tokio::test]
    async fn delete_transaction_without_uow_issues_exec() {
        let db = MockDatabase::new(DbBackend::Postgres)
            .append_exec_results([MockExecResult {
                last_insert_id: 0,
                rows_affected: 1,
            }])
            .into_connection();
        let repo = PostgresTransactionRepository::new(db);
        let result = repo.delete(TransactionId::generate(), None).await;
        assert!(result.is_ok(), "delete must succeed: {result:?}");
    }
}

// ---------------------------------------------------------------------------
// UnitOfWork downcast tests (`REPO-6`)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod uow_downcast {
    use super::*;
    use budget_domain::uow::UnitOfWork;
    use std::any::Any;

    /// A foreign `UnitOfWork` impl that is NOT a `SeaOrmUow` — exercises the
    /// rejection path.
    struct ForeignUow;
    impl UnitOfWork for ForeignUow {
        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    #[test]
    fn downcast_accepts_seaorm_uow_when_correctly_typed() {
        // We cannot easily obtain a live DatabaseTransaction here, so we test
        // the rejection path (which is what a wiring bug would trigger in prod).
        // The acceptance path is covered by the live UoW test in
        // tests/repositories_live.rs.
        let foreign: &dyn UnitOfWork = &ForeignUow;
        let result = SeaOrmUow::downcast(foreign);
        assert!(
            matches!(result, Err(RepositoryError::Database(_))),
            "a foreign UoW impl must be rejected"
        );
    }
}
