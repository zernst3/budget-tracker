//! Live-Postgres integration tests for the repository layer (build step 3).
//!
//! Gated on `DATABASE_URL` (no-op in CI until Neon is provisioned, SPEC §12).
//! When set, the suite proves:
//!   1. a user save -> find round-trip returns domain types (`REPO-2`),
//!   2. the SQL aggregates (`category_spent_for_month`, `month_net`) honor the
//!      inclusion polarity (`BUDGET-STATUS-DRIVES-INCLUSION-1`: settled +
//!      expected count, pending excluded) and aggregate in one query
//!      (`DB-NPLUSONE-1`),
//!   3. the unit of work rolls back on a closure error (`REPO-6`/`REPO-10`).
//!
//! Run against a throwaway database:
//!
//! ```text
//! DATABASE_URL=postgres://localhost/budget_repo_test cargo test -p budget-infrastructure
//! ```
//!
//! ### Why `connect_fresh` instead of `.clone()`
//!
//! `SeaORM`'s `DatabaseConnection` is `Clone` only when the `mock` feature is
//! disabled. This crate enables `mock` in `[dev-dependencies]` so the lib
//! test module (`src/repositories/mock_tests.rs`) can use `MockDatabase`.
//! That feature propagates to all test targets via Cargo feature unification,
//! removing `Clone` from `DatabaseConnection`. Each test therefore creates its
//! own pool connection from the same URL rather than cloning a shared one.
//! The overhead is negligible for an integration test that needs a live DB.

// Test-only: panicking is the correct failure signal against a live DB; the
// workspace denies panic in production code, this target is exempt by intent.
#![allow(clippy::panic)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use chrono::{NaiveDate, Utc};

use budget_domain::RepositoryError;
use budget_domain::budget::Budget;
use budget_domain::category::Category;
use budget_domain::enums::{
    Cadence, CategoryGrp, MonthStatus, TransactionSource, TransactionStatus,
};
use budget_domain::ids::{BudgetId, CategoryId, CategoryKey, MonthId, TransactionId, UserId};
use budget_domain::money::Money;
use budget_domain::month::Month;
use budget_domain::repositories::{
    BudgetRepository, MonthRepository, TransactionRepository, UserRepository,
};
use budget_domain::transaction::Transaction;
use budget_domain::uow::UowProvider;
use budget_domain::user::User;
use budget_domain::validated::Email;

use budget_infrastructure::{
    PostgresBudgetRepository, PostgresMonthRepository, PostgresTransactionRepository,
    PostgresUserRepository, SeaOrmUowProvider,
};
use budget_migration::{Migrator, MigratorTrait};
use sea_orm::{Database, DatabaseConnection};

/// Open a fresh connection pool to `url`.
///
/// Called by each test that needs a separate `DatabaseConnection` (see module
/// doc for why `.clone()` is no longer available).
async fn connect_fresh(url: &str) -> DatabaseConnection {
    Database::connect(url)
        .await
        .unwrap_or_else(|e| panic!("connect to {url}: {e}"))
}

/// Return the `DATABASE_URL` value if set, or `None` to skip the test.
///
/// The first call also resets the schema (fresh migrations) so each test suite
/// run starts from a clean slate.  Individual tests call this first and bail
/// early when `DATABASE_URL` is absent.
async fn setup() -> Option<String> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let db = connect_fresh(&url).await;
    Migrator::fresh(&db)
        .await
        .unwrap_or_else(|e| panic!("fresh schema: {e}"));
    Some(url)
}

fn sample_user(id: UserId) -> User {
    User {
        id,
        email: Email::try_new("zach@example.com").expect("valid email"),
        password_hash: "$argon2id$hash".to_string(),
        totp_secret: None,
        tracking_start_date: NaiveDate::from_ymd_opt(2026, 1, 1).expect("valid date"),
        created_at: Utc::now(),
    }
}

fn sample_budget(id: BudgetId, user_id: UserId) -> Budget {
    Budget {
        id,
        user_id,
        name: "Test Budget".to_string(),
        effective_from: NaiveDate::from_ymd_opt(2026, 1, 1).expect("valid date"),
        effective_to: None,
        created_at: Utc::now(),
    }
}

fn sample_category(id: CategoryId, budget_id: BudgetId, rollover: bool) -> Category {
    Category {
        id,
        budget_id,
        category_key: CategoryKey::generate(),
        name: if rollover {
            "Other".into()
        } else {
            "Groceries".into()
        },
        amount: Money::from_major(-500),
        grp: CategoryGrp::Discretionary,
        settle_type: None,
        expected_bills: None,
        is_rollover_bucket: rollover,
        cadence: Cadence::Monthly,
        period_months: None,
        fund_balance: Money::ZERO,
        next_due_date: None,
        sort_order: 0,
    }
}

fn sample_month(id: MonthId, user_id: UserId, budget_id: BudgetId) -> Month {
    Month {
        id,
        user_id,
        budget_id,
        year: 2026,
        month: 2,
        status: MonthStatus::Open,
        opened_at: Utc::now(),
        closed_at: None,
    }
}

#[allow(clippy::too_many_arguments)]
fn sample_txn(
    id: TransactionId,
    user_id: UserId,
    month_id: MonthId,
    category_id: CategoryId,
    amount: Money,
    status: TransactionStatus,
) -> Transaction {
    Transaction {
        id,
        user_id,
        month_id,
        category_id: Some(category_id),
        account_id: None,
        date: NaiveDate::from_ymd_opt(2026, 2, 10).expect("valid date"),
        amount,
        description: "test".to_string(),
        source: TransactionSource::Manual,
        plaid_transaction_id: None,
        status,
        income_kind: None,
        is_rollover: false,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    }
}

#[tokio::test]
async fn user_save_and_find_round_trips_domain_types() {
    let Some(url) = setup().await else {
        eprintln!("DATABASE_URL unset — skipping live repository test");
        return;
    };

    let db = connect_fresh(&url).await;
    let repo = PostgresUserRepository::new(db);
    let id = UserId::generate();
    let user = sample_user(id);

    repo.save(&user, None).await.expect("save user");

    let found = repo
        .find_by_id(id)
        .await
        .expect("find by id")
        .expect("present");
    assert_eq!(found.id, id);
    assert_eq!(found.email.as_str(), "zach@example.com");

    let by_email = repo
        .find_by_email("zach@example.com")
        .await
        .expect("find by email")
        .expect("present");
    assert_eq!(by_email.id, id);

    // Upsert: re-saving with a changed field updates rather than duplicating.
    let mut updated = user.clone();
    updated.password_hash = "$argon2id$rotated".to_string();
    repo.save(&updated, None).await.expect("upsert user");
    let refound = repo.find_by_id(id).await.expect("refind").expect("present");
    assert_eq!(refound.password_hash, "$argon2id$rotated");
}

#[tokio::test]
async fn aggregates_honor_inclusion_polarity_in_one_query() {
    let Some(url) = setup().await else {
        eprintln!("DATABASE_URL unset — skipping live aggregate test");
        return;
    };

    let user_id = UserId::generate();
    let budget_id = BudgetId::generate();
    let category_id = CategoryId::generate();
    let month_id = MonthId::generate();

    // Each repo gets its own fresh pool connection (see module doc).
    let users = PostgresUserRepository::new(connect_fresh(&url).await);
    let budgets = PostgresBudgetRepository::new(connect_fresh(&url).await);
    let months = PostgresMonthRepository::new(connect_fresh(&url).await);
    let txns = PostgresTransactionRepository::new(connect_fresh(&url).await);

    users.save(&sample_user(user_id), None).await.expect("user");
    budgets
        .save(&sample_budget(budget_id, user_id), None)
        .await
        .expect("budget");
    budgets
        .save_category(&sample_category(category_id, budget_id, false), None)
        .await
        .expect("category");
    months
        .save(&sample_month(month_id, user_id, budget_id), None)
        .await
        .expect("month");

    // settled -$100, expected -$40 (both count), pending -$999 (excluded).
    txns.save(
        &sample_txn(
            TransactionId::generate(),
            user_id,
            month_id,
            category_id,
            Money::from_major(-100),
            TransactionStatus::Settled,
        ),
        None,
    )
    .await
    .expect("settled txn");
    txns.save(
        &sample_txn(
            TransactionId::generate(),
            user_id,
            month_id,
            category_id,
            Money::from_major(-40),
            TransactionStatus::Expected,
        ),
        None,
    )
    .await
    .expect("expected txn");
    txns.save(
        &sample_txn(
            TransactionId::generate(),
            user_id,
            month_id,
            category_id,
            Money::from_major(-999),
            TransactionStatus::Pending,
        ),
        None,
    )
    .await
    .expect("pending txn");

    // category_spent_for_month: -100 + -40 = -140 (pending excluded).
    let spent = txns
        .category_spent_for_month(month_id)
        .await
        .expect("category spent");
    assert_eq!(spent.len(), 1, "one category bucket");
    assert_eq!(spent[0].category_id, category_id);
    assert_eq!(spent[0].spent, Money::from_major(-140));

    // month_net: same polarity, single scalar.
    let net = txns.month_net(month_id).await.expect("month net");
    assert_eq!(net.month_id, month_id);
    assert_eq!(net.net, Money::from_major(-140));

    // An empty month nets to zero, never None.
    let empty_month_id = MonthId::generate();
    let empty_month = Month {
        id: empty_month_id,
        month: 3,
        ..sample_month(empty_month_id, user_id, budget_id)
    };
    months.save(&empty_month, None).await.expect("empty month");
    let empty_net = txns.month_net(empty_month_id).await.expect("empty net");
    assert_eq!(empty_net.net, Money::ZERO);
}

#[tokio::test]
async fn unit_of_work_rolls_back_on_closure_error() {
    let Some(url) = setup().await else {
        eprintln!("DATABASE_URL unset — skipping live UoW test");
        return;
    };

    let user_id = UserId::generate();
    let users = PostgresUserRepository::new(connect_fresh(&url).await);
    let provider = SeaOrmUowProvider::new(connect_fresh(&url).await);

    // A UoW closure that saves a user then returns Err must leave NO user row.
    // The provider's `run` requires the closure be `Send + 'static`, so it owns a
    // fresh connection + user and builds its own repo handle inside.
    let user = sample_user(user_id);
    let url_for_closure = url.clone();
    let result: Result<(), RepositoryError> = provider
        .run(move |uow| {
            let url2 = url_for_closure.clone();
            Box::pin(async move {
                let inner_db = connect_fresh(&url2).await;
                let inner_repo = PostgresUserRepository::new(inner_db);
                inner_repo.save(&user, Some(uow)).await?;
                Err(RepositoryError::Database("forced rollback".to_string()))
            })
        })
        .await;
    assert!(result.is_err(), "closure error propagates");

    let found = users.find_by_id(user_id).await.expect("find");
    assert!(found.is_none(), "user must have been rolled back");

    // A UoW closure that succeeds commits the row.
    let committed_id = UserId::generate();
    let committed_user = sample_user(committed_id);
    let url_for_commit = url.clone();
    let committed: Result<(), RepositoryError> = provider
        .run(move |uow| {
            let url2 = url_for_commit.clone();
            Box::pin(async move {
                let inner_db = connect_fresh(&url2).await;
                let inner_repo = PostgresUserRepository::new(inner_db);
                inner_repo.save(&committed_user, Some(uow)).await
            })
        })
        .await;
    committed.expect("commit");
    assert!(
        users
            .find_by_id(committed_id)
            .await
            .expect("find")
            .is_some(),
        "committed user must be present"
    );
}
