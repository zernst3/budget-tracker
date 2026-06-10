//! Unit tests for the month-lifecycle service.
//!
//! Use DB-free in-memory fakes (a `Mutex`-wrapped store per aggregate) so the
//! lazy-init catch-up, rollover idempotency, and the `D5` netting can be tested
//! without Postgres. The fakes reproduce the two DB invariants the service
//! leans on: `create_if_absent` dedups on `(user_id, year, month)`
//! (`BUDGET-IDEMPOTENT-MONTH-INIT-1`) and the transaction store rejects a second
//! `is_rollover` row for the same month with a `UniqueViolation`
//! (`BUDGET-ROLLOVER-INTEGRITY-1`).
//!
//! ### Lint suppressions (test-only)
//!
//! The workspace denies `unwrap_used`, `expect_used`, and `panic` in production
//! code. Test code intentionally panics on assertion failure; these lints are
//! suppressed for this module only, matching the infra mock-test convention.

#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]
#![allow(clippy::panic)]

use std::any::Any;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::{Datelike, NaiveDate, TimeZone, Utc};

use budget_domain::budget::Budget;
use budget_domain::category::Category;
use budget_domain::enums::{
    Cadence, CategoryGrp, IncomeKind, TransactionSource, TransactionStatus,
};
use budget_domain::fund::Fund;
use budget_domain::ids::{
    BudgetId, CategoryId, CategoryKey, FundId, MonthId, RepaymentObligationId, TransactionId,
    UserId,
};
use budget_domain::money::Money;
use budget_domain::month::Month;
use budget_domain::repayment_obligation::RepaymentObligation;
use budget_domain::repositories::{
    BudgetRepository, FundRepository, MonthRepository, TransactionRepository,
};
use budget_domain::transaction::Transaction;
use budget_domain::uow::{UnitOfWork, UowFuture, UowProvider};
use budget_domain::{CategorySpent, MonthNet, RepositoryError};

use crate::income::SemimonthlyFixedExpectation;

use super::*;

// ---------------------------------------------------------------------------
// In-memory fakes
// ---------------------------------------------------------------------------

/// A no-op unit-of-work handle: the fakes ignore the `uow` argument entirely
/// (there is no real transaction to enlist in), so the handle only needs to
/// satisfy the `as_any` downcast surface.
struct FakeUow;
impl UnitOfWork for FakeUow {
    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// The boxed closure body the domain's `UowProvider::run_boxed` receives.
///
/// Spelled out here (the domain keeps the alias private) so the fake's
/// `run_boxed` signature matches the trait exactly, mirroring the real
/// `SeaOrmUowProvider`.
type BoxedUowClosure<'a> =
    Box<dyn for<'u> FnOnce(&'u dyn UnitOfWork) -> UowFuture<'u, Box<dyn Any + Send>> + Send + 'a>;

/// A `UowProvider` that simply runs the closure with a no-op handle (no real
/// transaction). The service's atomicity contract is exercised structurally;
/// real commit/rollback is a repository-integration concern covered by the
/// infra live tests.
struct FakeUowProvider;

#[async_trait]
impl UowProvider for FakeUowProvider {
    async fn run_boxed(
        &self,
        f: BoxedUowClosure<'_>,
    ) -> Result<Box<dyn Any + Send>, RepositoryError> {
        let uow = FakeUow;
        let handle: &dyn UnitOfWork = &uow;
        f(handle).await
    }
}

#[derive(Default)]
struct MonthStore {
    months: Vec<Month>,
}

struct FakeMonthRepo {
    store: Mutex<MonthStore>,
}

impl FakeMonthRepo {
    fn new() -> Self {
        Self {
            store: Mutex::new(MonthStore::default()),
        }
    }
}

#[async_trait]
impl MonthRepository for FakeMonthRepo {
    async fn find_by_id(&self, id: MonthId) -> Result<Option<Month>, RepositoryError> {
        let store = self.store.lock().map_err(poisoned)?;
        Ok(store.months.iter().find(|m| m.id == id).cloned())
    }

    async fn find_by_year_month(
        &self,
        user_id: UserId,
        year: i32,
        month: i32,
    ) -> Result<Option<Month>, RepositoryError> {
        let store = self.store.lock().map_err(poisoned)?;
        Ok(store
            .months
            .iter()
            .find(|m| m.user_id == user_id && m.year == year && m.month == month)
            .cloned())
    }

    async fn find_latest(&self, user_id: UserId) -> Result<Option<Month>, RepositoryError> {
        let store = self.store.lock().map_err(poisoned)?;
        Ok(store
            .months
            .iter()
            .filter(|m| m.user_id == user_id)
            .max_by_key(|m| Month::sort_key(m))
            .cloned())
    }

    async fn list_for_user(&self, user_id: UserId) -> Result<Vec<Month>, RepositoryError> {
        let store = self.store.lock().map_err(poisoned)?;
        let mut v: Vec<Month> = store
            .months
            .iter()
            .filter(|m| m.user_id == user_id)
            .cloned()
            .collect();
        v.sort_by_key(Month::sort_key);
        Ok(v)
    }

    async fn create_if_absent(
        &self,
        month: &Month,
        _uow: Option<&dyn UnitOfWork>,
    ) -> Result<Month, RepositoryError> {
        // BUDGET-IDEMPOTENT-MONTH-INIT-1: dedup on (user_id, year, month).
        let mut store = self.store.lock().map_err(poisoned)?;
        if let Some(existing) = store
            .months
            .iter()
            .find(|m| m.user_id == month.user_id && m.year == month.year && m.month == month.month)
        {
            return Ok(existing.clone());
        }
        store.months.push(month.clone());
        Ok(month.clone())
    }

    async fn save(
        &self,
        month: &Month,
        _uow: Option<&dyn UnitOfWork>,
    ) -> Result<(), RepositoryError> {
        let mut store = self.store.lock().map_err(poisoned)?;
        if let Some(slot) = store.months.iter_mut().find(|m| m.id == month.id) {
            *slot = month.clone();
        } else {
            store.months.push(month.clone());
        }
        Ok(())
    }
}

#[derive(Default)]
struct BudgetStore {
    budgets: Vec<Budget>,
    categories: Vec<Category>,
}

struct FakeBudgetRepo {
    store: Mutex<BudgetStore>,
}

impl FakeBudgetRepo {
    fn new() -> Self {
        Self {
            store: Mutex::new(BudgetStore::default()),
        }
    }
}

#[async_trait]
impl BudgetRepository for FakeBudgetRepo {
    async fn find_by_id(&self, id: BudgetId) -> Result<Option<Budget>, RepositoryError> {
        let store = self.store.lock().map_err(poisoned)?;
        Ok(store.budgets.iter().find(|b| b.id == id).cloned())
    }

    async fn find_active_for_date(
        &self,
        user_id: UserId,
        date: NaiveDate,
    ) -> Result<Option<Budget>, RepositoryError> {
        let store = self.store.lock().map_err(poisoned)?;
        Ok(store
            .budgets
            .iter()
            .find(|b| {
                b.user_id == user_id
                    && b.effective_from <= date
                    && b.effective_to.is_none_or(|to| date <= to)
            })
            .cloned())
    }

    async fn find_current(&self, user_id: UserId) -> Result<Option<Budget>, RepositoryError> {
        let store = self.store.lock().map_err(poisoned)?;
        Ok(store
            .budgets
            .iter()
            .find(|b| b.user_id == user_id && b.effective_to.is_none())
            .cloned())
    }

    async fn list_for_user(&self, user_id: UserId) -> Result<Vec<Budget>, RepositoryError> {
        let store = self.store.lock().map_err(poisoned)?;
        Ok(store
            .budgets
            .iter()
            .filter(|b| b.user_id == user_id)
            .cloned()
            .collect())
    }

    async fn list_categories(&self, budget_id: BudgetId) -> Result<Vec<Category>, RepositoryError> {
        let store = self.store.lock().map_err(poisoned)?;
        Ok(store
            .categories
            .iter()
            .filter(|c| c.budget_id == budget_id)
            .cloned()
            .collect())
    }

    async fn find_category(&self, id: CategoryId) -> Result<Option<Category>, RepositoryError> {
        let store = self.store.lock().map_err(poisoned)?;
        Ok(store.categories.iter().find(|c| c.id == id).cloned())
    }

    async fn find_rollover_bucket(
        &self,
        budget_id: BudgetId,
    ) -> Result<Option<Category>, RepositoryError> {
        let store = self.store.lock().map_err(poisoned)?;
        Ok(store
            .categories
            .iter()
            .find(|c| c.budget_id == budget_id && c.is_rollover_bucket)
            .cloned())
    }

    async fn save(
        &self,
        budget: &Budget,
        _uow: Option<&dyn UnitOfWork>,
    ) -> Result<(), RepositoryError> {
        let mut store = self.store.lock().map_err(poisoned)?;
        if let Some(slot) = store.budgets.iter_mut().find(|b| b.id == budget.id) {
            *slot = budget.clone();
        } else {
            store.budgets.push(budget.clone());
        }
        Ok(())
    }

    async fn save_category(
        &self,
        category: &Category,
        _uow: Option<&dyn UnitOfWork>,
    ) -> Result<(), RepositoryError> {
        let mut store = self.store.lock().map_err(poisoned)?;
        if let Some(slot) = store.categories.iter_mut().find(|c| c.id == category.id) {
            *slot = category.clone();
        } else {
            store.categories.push(category.clone());
        }
        Ok(())
    }
}

#[derive(Default)]
struct TxnStore {
    txns: Vec<Transaction>,
}

struct FakeTransactionRepo {
    store: Mutex<TxnStore>,
}

impl FakeTransactionRepo {
    fn new() -> Self {
        Self {
            store: Mutex::new(TxnStore::default()),
        }
    }

    fn rollover_count(&self, month_id: MonthId) -> usize {
        let store = self.store.lock().unwrap();
        store
            .txns
            .iter()
            .filter(|t| t.month_id == month_id && t.is_rollover)
            .count()
    }
}

#[async_trait]
impl TransactionRepository for FakeTransactionRepo {
    async fn find_by_id(&self, id: TransactionId) -> Result<Option<Transaction>, RepositoryError> {
        let store = self.store.lock().map_err(poisoned)?;
        Ok(store.txns.iter().find(|t| t.id == id).cloned())
    }

    async fn list_for_month(&self, month_id: MonthId) -> Result<Vec<Transaction>, RepositoryError> {
        let store = self.store.lock().map_err(poisoned)?;
        Ok(store
            .txns
            .iter()
            .filter(|t| t.month_id == month_id)
            .cloned()
            .collect())
    }

    async fn list_pending_inbox(
        &self,
        _user_id: UserId,
    ) -> Result<Vec<Transaction>, RepositoryError> {
        Ok(vec![])
    }

    async fn list_for_category_in_month(
        &self,
        month_id: MonthId,
        category_id: CategoryId,
    ) -> Result<Vec<Transaction>, RepositoryError> {
        let store = self.store.lock().map_err(poisoned)?;
        Ok(store
            .txns
            .iter()
            .filter(|t| t.month_id == month_id && t.category_id == Some(category_id))
            .cloned()
            .collect())
    }

    async fn find_rollover_for_month(
        &self,
        month_id: MonthId,
    ) -> Result<Option<Transaction>, RepositoryError> {
        let store = self.store.lock().map_err(poisoned)?;
        Ok(store
            .txns
            .iter()
            .find(|t| t.month_id == month_id && t.is_rollover)
            .cloned())
    }

    async fn find_by_plaid_transaction_id(
        &self,
        plaid_transaction_id: &str,
    ) -> Result<Option<Transaction>, RepositoryError> {
        let store = self.store.lock().map_err(poisoned)?;
        Ok(store
            .txns
            .iter()
            .find(|t| t.plaid_transaction_id.as_deref() == Some(plaid_transaction_id))
            .cloned())
    }

    async fn list_expected_for_month(
        &self,
        month_id: MonthId,
    ) -> Result<Vec<Transaction>, RepositoryError> {
        let store = self.store.lock().map_err(poisoned)?;
        Ok(store
            .txns
            .iter()
            .filter(|t| t.month_id == month_id && t.status == TransactionStatus::Expected)
            .cloned()
            .collect())
    }

    async fn find_expected_matched_to(
        &self,
        real_transaction_id: TransactionId,
    ) -> Result<Option<Transaction>, RepositoryError> {
        let store = self.store.lock().map_err(poisoned)?;
        Ok(store
            .txns
            .iter()
            .find(|t| t.matched_transaction_id == Some(real_transaction_id))
            .cloned())
    }

    async fn category_spent_for_month(
        &self,
        _month_id: MonthId,
    ) -> Result<Vec<CategorySpent>, RepositoryError> {
        Ok(Vec::new())
    }

    async fn month_net(&self, month_id: MonthId) -> Result<MonthNet, RepositoryError> {
        let store = self.store.lock().map_err(poisoned)?;
        let net: Money = store
            .txns
            .iter()
            .filter(|t| t.month_id == month_id && counts_in_budget(t.status))
            .map(|t| t.amount)
            .sum();
        Ok(MonthNet { month_id, net })
    }

    async fn save(
        &self,
        transaction: &Transaction,
        _uow: Option<&dyn UnitOfWork>,
    ) -> Result<(), RepositoryError> {
        let mut store = self.store.lock().map_err(poisoned)?;
        // Partial-unique transactions(month_id) WHERE is_rollover: reject a
        // second rollover for the same month (BUDGET-ROLLOVER-INTEGRITY-1).
        if transaction.is_rollover
            && store.txns.iter().any(|t| {
                t.month_id == transaction.month_id && t.is_rollover && t.id != transaction.id
            })
        {
            return Err(RepositoryError::UniqueViolation(
                "transactions(month_id) WHERE is_rollover".to_owned(),
            ));
        }
        if let Some(slot) = store.txns.iter_mut().find(|t| t.id == transaction.id) {
            *slot = transaction.clone();
        } else {
            store.txns.push(transaction.clone());
        }
        Ok(())
    }

    async fn delete(
        &self,
        id: TransactionId,
        _uow: Option<&dyn UnitOfWork>,
    ) -> Result<(), RepositoryError> {
        let mut store = self.store.lock().map_err(poisoned)?;
        store.txns.retain(|t| t.id != id);
        Ok(())
    }
}

/// A fund repo holding obligations so the netting can exclude buffer-financed
/// full-price transactions (`SPEC §4.9` D7). Funds themselves are unused by the
/// month-lifecycle netting, so only the obligation surface is meaningful.
#[derive(Default)]
struct FundStore {
    funds: Vec<Fund>,
    obligations: Vec<RepaymentObligation>,
}

struct FakeFundRepo {
    store: Mutex<FundStore>,
}

impl FakeFundRepo {
    fn new() -> Self {
        Self {
            store: Mutex::new(FundStore::default()),
        }
    }
}

#[async_trait]
impl FundRepository for FakeFundRepo {
    async fn find_by_id(&self, id: FundId) -> Result<Option<Fund>, RepositoryError> {
        let store = self.store.lock().map_err(poisoned)?;
        Ok(store.funds.iter().find(|f| f.id == id).cloned())
    }

    async fn list_for_user(&self, user_id: UserId) -> Result<Vec<Fund>, RepositoryError> {
        let store = self.store.lock().map_err(poisoned)?;
        Ok(store
            .funds
            .iter()
            .filter(|f| f.user_id == user_id)
            .cloned()
            .collect())
    }

    async fn save(
        &self,
        fund: &Fund,
        _uow: Option<&dyn UnitOfWork>,
    ) -> Result<(), RepositoryError> {
        let mut store = self.store.lock().map_err(poisoned)?;
        if let Some(slot) = store.funds.iter_mut().find(|f| f.id == fund.id) {
            *slot = fund.clone();
        } else {
            store.funds.push(fund.clone());
        }
        Ok(())
    }

    async fn find_obligation(
        &self,
        id: RepaymentObligationId,
    ) -> Result<Option<RepaymentObligation>, RepositoryError> {
        let store = self.store.lock().map_err(poisoned)?;
        Ok(store.obligations.iter().find(|o| o.id == id).cloned())
    }

    async fn list_active_obligations(
        &self,
        user_id: UserId,
    ) -> Result<Vec<RepaymentObligation>, RepositoryError> {
        let store = self.store.lock().map_err(poisoned)?;
        Ok(store
            .obligations
            .iter()
            .filter(|o| {
                o.user_id == user_id && o.status == budget_domain::enums::ObligationStatus::Active
            })
            .cloned()
            .collect())
    }

    async fn find_obligation_for_transaction(
        &self,
        transaction_id: TransactionId,
    ) -> Result<Option<RepaymentObligation>, RepositoryError> {
        let store = self.store.lock().map_err(poisoned)?;
        Ok(store
            .obligations
            .iter()
            .find(|o| o.transaction_id == Some(transaction_id))
            .cloned())
    }

    async fn find_deficit_obligation_for_month(
        &self,
        month_id: MonthId,
    ) -> Result<Option<RepaymentObligation>, RepositoryError> {
        // Regardless of status — mirrors the real repo (months==1 financing is Paid
        // immediately yet must still suppress rollover).
        let store = self.store.lock().map_err(poisoned)?;
        Ok(store
            .obligations
            .iter()
            .find(|o| {
                o.origin_month_id == Some(month_id)
                    && o.source == budget_domain::enums::ObligationSource::Deficit
            })
            .cloned())
    }

    async fn list_buffer_financed_transaction_ids(
        &self,
        user_id: UserId,
    ) -> Result<Vec<TransactionId>, RepositoryError> {
        let store = self.store.lock().map_err(poisoned)?;
        Ok(store
            .obligations
            .iter()
            .filter(|o| o.user_id == user_id)
            .filter_map(|o| o.transaction_id)
            .collect())
    }

    async fn save_obligation(
        &self,
        obligation: &RepaymentObligation,
        _uow: Option<&dyn UnitOfWork>,
    ) -> Result<(), RepositoryError> {
        let mut store = self.store.lock().map_err(poisoned)?;
        if let Some(slot) = store.obligations.iter_mut().find(|o| o.id == obligation.id) {
            *slot = obligation.clone();
        } else {
            store.obligations.push(obligation.clone());
        }
        Ok(())
    }
}

fn poisoned<T>(_e: std::sync::PoisonError<T>) -> RepositoryError {
    RepositoryError::Database("test mutex poisoned".to_owned())
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

struct Harness {
    months: Arc<FakeMonthRepo>,
    budgets: Arc<FakeBudgetRepo>,
    transactions: Arc<FakeTransactionRepo>,
    funds: Arc<FakeFundRepo>,
    service: MonthLifecycleService,
    user_id: UserId,
    budget_id: BudgetId,
    rollover_bucket_id: CategoryId,
}

fn ymd(y: i32, m: u32, d: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, d)
        .unwrap_or_else(|| NaiveDate::from_ymd_opt(2000, 1, 1).unwrap_or(NaiveDate::MIN))
}

/// Build a harness with one open-ended budget version + a rollover bucket and a
/// $2,000 semimonthly expectation (so expected income = $4,000/month).
fn harness() -> Harness {
    let months = Arc::new(FakeMonthRepo::new());
    let budgets = Arc::new(FakeBudgetRepo::new());
    let transactions = Arc::new(FakeTransactionRepo::new());
    let funds = Arc::new(FakeFundRepo::new());

    let user_id = UserId::generate();
    let budget_id = BudgetId::generate();
    let rollover_bucket_id = CategoryId::generate();

    {
        let mut store = budgets.store.lock().unwrap();
        store.budgets.push(Budget {
            id: budget_id,
            user_id,
            name: "Test Budget".to_owned(),
            effective_from: ymd(2020, 1, 1),
            effective_to: None,
            created_at: Utc::now(),
        });
        store.categories.push(Category {
            id: rollover_bucket_id,
            budget_id,
            category_key: CategoryKey::generate(),
            name: "Other".to_owned(),
            amount: Money::ZERO,
            grp: CategoryGrp::Discretionary,
            settle_type: None,
            expected_bills: None,
            is_rollover_bucket: true,
            cadence: Cadence::Monthly,
            period_months: None,
            fund_balance: Money::ZERO,
            next_due_date: None,
            sort_order: 0,
        });
    }

    let income = Arc::new(SemimonthlyFixedExpectation::new(Money::from_major(2_000)));

    let service = MonthLifecycleService::new(
        Arc::clone(&months) as Arc<dyn MonthRepository>,
        Arc::clone(&budgets) as Arc<dyn BudgetRepository>,
        Arc::clone(&transactions) as Arc<dyn TransactionRepository>,
        Arc::clone(&funds) as Arc<dyn FundRepository>,
        Arc::new(FakeUowProvider) as Arc<dyn UowProvider>,
        income,
    );

    Harness {
        months,
        budgets,
        transactions,
        funds,
        service,
        user_id,
        budget_id,
        rollover_bucket_id,
    }
}

/// A non-fund expense category, added to the harness budget.
fn add_expense_category(h: &Harness, name: &str) -> CategoryId {
    let id = CategoryId::generate();
    let mut store = h.budgets.store.lock().unwrap();
    store.categories.push(Category {
        id,
        budget_id: h.budget_id,
        category_key: CategoryKey::generate(),
        name: name.to_owned(),
        amount: Money::from_major(500),
        grp: CategoryGrp::Discretionary,
        settle_type: None,
        expected_bills: None,
        is_rollover_bucket: false,
        cadence: Cadence::Monthly,
        period_months: None,
        fund_balance: Money::ZERO,
        next_due_date: None,
        sort_order: 1,
    });
    id
}

/// A sinking-fund (annual) category, added to the harness budget.
fn add_fund_category(h: &Harness, name: &str) -> CategoryId {
    let id = CategoryId::generate();
    let mut store = h.budgets.store.lock().unwrap();
    store.categories.push(Category {
        id,
        budget_id: h.budget_id,
        category_key: CategoryKey::generate(),
        name: name.to_owned(),
        amount: Money::from_major(1_200),
        grp: CategoryGrp::Fixed,
        settle_type: None,
        expected_bills: None,
        is_rollover_bucket: false,
        cadence: Cadence::Annual,
        period_months: None,
        fund_balance: Money::ZERO,
        next_due_date: None,
        sort_order: 2,
    });
    id
}

/// Push a transaction directly into a month (bypassing the service).
fn push_txn(h: &Harness, t: Transaction) {
    let mut store = h.transactions.store.lock().unwrap();
    store.txns.push(t);
}

fn base_txn(h: &Harness, month_id: MonthId, amount: Money) -> Transaction {
    Transaction {
        id: TransactionId::generate(),
        user_id: h.user_id,
        month_id,
        category_id: None,
        account_id: None,
        date: ymd(2026, 1, 15),
        amount,
        description: "t".to_owned(),
        source: TransactionSource::Manual,
        plaid_transaction_id: None,
        status: TransactionStatus::Settled,
        income_kind: None,
        is_rollover: false,
        is_fund_draw: false,
        matched_transaction_id: None,
        comment: None,
        is_transfer: false,
        plaid_category: None,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    }
}

/// A UTC instant that is `(year, month, day)` at `hour:00` New-York local time.
fn ny_local(year: i32, month: u32, day: u32, hour: u32) -> chrono::DateTime<Utc> {
    let naive = ymd(year, month, day).and_hms_opt(hour, 0, 0).unwrap();
    New_York
        .from_local_datetime(&naive)
        .single()
        .unwrap()
        .with_timezone(&Utc)
}

/// A UTC instant that is `(year, month, day)` noon in New York.
fn ny_noon(year: i32, month: u32, day: u32) -> chrono::DateTime<Utc> {
    ny_local(year, month, day, 12)
}

// ---------------------------------------------------------------------------
// net_leftover (D5 formula)
// ---------------------------------------------------------------------------

#[test]
fn net_leftover_combines_income_variance_and_expense_remaining() {
    // D5: (actual - expected) + Σ remaining.
    // actual $4,100, expected $4,000 -> +$100 income surplus; expenses -$300.
    let net = net_leftover(
        Money::from_major(4_100),
        Money::from_major(4_000),
        Money::from_major(-300),
    );
    assert_eq!(net, Money::from_major(-200));
}

#[test]
fn net_leftover_income_surplus_raises_other_by_formula() {
    // D5: a higher-than-expected paycheck raises Other by the surplus, no line item.
    let with_expected = net_leftover(
        Money::from_major(4_000),
        Money::from_major(4_000),
        Money::ZERO,
    );
    let with_surplus = net_leftover(
        Money::from_major(4_250),
        Money::from_major(4_000),
        Money::ZERO,
    );
    assert_eq!(with_expected, Money::ZERO);
    assert_eq!(with_surplus, Money::from_major(250));
}

// ---------------------------------------------------------------------------
// expense_remaining_sum + fund contributions COUNT (D6 Model A / BUDGET-FUND-EARMARK-1)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rollover_counts_fund_contributions() {
    // BUDGET-FUND-EARMARK-1 (D6 Model A): a $100 fund contribution is a manual
    // Other-bucket expense that COUNTS in the net (is_fund_draw=false), exactly like
    // an ordinary $100 expense. Both count, so the net is −$200.
    let h = harness();
    let fund_cat = add_fund_category(&h, "Insurance fund");
    let exp_cat = add_expense_category(&h, "Groceries");

    // Seed Jan 2026 as the latest month with two expenses + matching income so
    // only the fund-vs-ordinary distinction drives the net.
    let jan = h
        .service
        .ensure_current_month(h.user_id, ny_noon(2026, 1, 10))
        .await
        .expect("init jan");

    // Income exactly meets expectation (no income variance).
    let mut income = base_txn(&h, jan.id, Money::from_major(4_000));
    income.income_kind = Some(IncomeKind::Budgeted);
    push_txn(&h, income);

    // $100 ordinary expense (counts) + $100 fund contribution (also counts under
    // Model A: a manual Other-bucket expense, is_fund_draw=false).
    let mut ordinary = base_txn(&h, jan.id, Money::from_major(-100));
    ordinary.category_id = Some(exp_cat);
    push_txn(&h, ordinary);

    let mut fund = base_txn(&h, jan.id, Money::from_major(-100));
    fund.category_id = Some(fund_cat);
    push_txn(&h, fund);

    // Advance to Feb; the Jan->Feb rollover should be -$200 (both the ordinary
    // expense AND the fund contribution count under D6 Model A).
    h.service
        .ensure_current_month(h.user_id, ny_noon(2026, 2, 10))
        .await
        .expect("init feb");

    let feb = h
        .months
        .find_by_year_month(h.user_id, 2026, 2)
        .await
        .expect("feb lookup")
        .expect("feb exists");
    let rollover = h
        .transactions
        .find_rollover_for_month(feb.id)
        .await
        .expect("rollover lookup")
        .expect("rollover exists");
    assert_eq!(
        rollover.amount,
        Money::from_major(-200),
        "fund contribution COUNTS in the rollover net (D6 Model A): ordinary + contribution"
    );
    assert!(rollover.is_rollover);
    assert_eq!(rollover.category_id, Some(h.rollover_bucket_id));
    assert_eq!(rollover.date, ymd(2026, 2, 1));
}

#[tokio::test]
async fn rollover_excludes_buffer_financed_full_price_but_includes_installment() {
    // SPEC §4.9 D7: a buffer-financed full-price purchase posts for tracking with
    // zero month-budget impact (excluded from the rollover net because an
    // obligation references it), while the installment IS a counted month expense.
    let h = harness();
    let exp_cat = add_expense_category(&h, "Groceries");

    let jan = h
        .service
        .ensure_current_month(h.user_id, ny_noon(2026, 1, 10))
        .await
        .expect("init jan");

    // Income meets expectation exactly (no income variance).
    let mut income = base_txn(&h, jan.id, Money::from_major(4_000));
    income.income_kind = Some(IncomeKind::Budgeted);
    push_txn(&h, income);

    // The full-price buffer-financed tracking row: -$1,200, uncategorized.
    let full_price = base_txn(&h, jan.id, Money::from_major(-1_200));
    let full_price_id = full_price.id;
    push_txn(&h, full_price);

    // Register the obligation referencing that txn so the netting excludes it.
    h.funds
        .store
        .lock()
        .unwrap()
        .obligations
        .push(RepaymentObligation {
            id: RepaymentObligationId::generate(),
            user_id: h.user_id,
            fund_id: FundId::generate(),
            source: budget_domain::enums::ObligationSource::LargePurchase,
            transaction_id: Some(full_price_id),
            origin_month_id: None,
            total_amount: Money::from_major(1_200),
            remaining_amount: Money::from_major(1_100),
            installment_amount: Money::from_major(100),
            months_remaining: 11,
            status: budget_domain::enums::ObligationStatus::Active,
            created_at: Utc::now(),
        });

    // A -$100 installment (a counted month expense) flowing back into the buffer.
    let mut installment = base_txn(&h, jan.id, Money::from_major(-100));
    installment.category_id = Some(exp_cat);
    push_txn(&h, installment);

    h.service
        .ensure_current_month(h.user_id, ny_noon(2026, 2, 10))
        .await
        .expect("init feb");

    let feb = h
        .months
        .find_by_year_month(h.user_id, 2026, 2)
        .await
        .expect("feb lookup")
        .expect("feb exists");
    let rollover = h
        .transactions
        .find_rollover_for_month(feb.id)
        .await
        .expect("rollover lookup")
        .expect("rollover exists");
    // The $1,200 full price is EXCLUDED; only the -$100 installment nets. Were the
    // full price counted, the rollover would be -$1,300.
    assert_eq!(
        rollover.amount,
        Money::from_major(-100),
        "buffer-financed full price excluded; installment included"
    );
}

// ---------------------------------------------------------------------------
// Lazy init: genesis, single advance, multi-month gap
// ---------------------------------------------------------------------------

#[tokio::test]
async fn genesis_creates_current_month_with_zero_rollover() {
    let h = harness();
    let m = h
        .service
        .ensure_current_month(h.user_id, ny_noon(2026, 3, 15))
        .await
        .expect("genesis init");
    assert_eq!((m.year, m.month), (2026, 3));
    // First month: rollover present but zero (no prior month).
    let rollover = h
        .transactions
        .find_rollover_for_month(m.id)
        .await
        .expect("lookup")
        .expect("genesis rollover present");
    assert_eq!(rollover.amount, Money::ZERO);
}

#[tokio::test]
async fn multi_month_gap_creates_every_missing_month_in_order() {
    // SPEC §4.6: container asleep for months. Init at Jan, then jump to May:
    // Feb, Mar, Apr, May must all be created, each with a rollover.
    let h = harness();
    h.service
        .ensure_current_month(h.user_id, ny_noon(2026, 1, 10))
        .await
        .expect("init jan");

    h.service
        .ensure_current_month(h.user_id, ny_noon(2026, 5, 10))
        .await
        .expect("catch up to may");

    let all = h.months.list_for_user(h.user_id).await.expect("list");
    let keys: Vec<(i32, i32)> = all.iter().map(|m| (m.year, m.month)).collect();
    assert_eq!(
        keys,
        vec![(2026, 1), (2026, 2), (2026, 3), (2026, 4), (2026, 5)],
        "every missing month created in chronological order"
    );
    // Every month has exactly one rollover row.
    for m in &all {
        assert_eq!(
            h.transactions.rollover_count(m.id),
            1,
            "month {}-{} has exactly one rollover",
            m.year,
            m.month
        );
    }
}

#[tokio::test]
async fn year_boundary_gap_wraps_december_to_january() {
    let h = harness();
    h.service
        .ensure_current_month(h.user_id, ny_noon(2025, 11, 10))
        .await
        .expect("init nov 2025");
    h.service
        .ensure_current_month(h.user_id, ny_noon(2026, 1, 10))
        .await
        .expect("catch up over year boundary");
    let all = h.months.list_for_user(h.user_id).await.expect("list");
    let keys: Vec<(i32, i32)> = all.iter().map(|m| (m.year, m.month)).collect();
    assert_eq!(keys, vec![(2025, 11), (2025, 12), (2026, 1)]);
}

// ---------------------------------------------------------------------------
// Idempotency (BUDGET-IDEMPOTENT-MONTH-INIT-1 / BUDGET-ROLLOVER-INTEGRITY-1)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn re_entry_does_not_double_create_or_double_post() {
    let h = harness();
    // Run the same init three times at the same instant.
    for _ in 0..3 {
        h.service
            .ensure_current_month(h.user_id, ny_noon(2026, 4, 20))
            .await
            .expect("idempotent init");
    }
    let all = h.months.list_for_user(h.user_id).await.expect("list");
    assert_eq!(all.len(), 1, "exactly one month despite 3 inits");
    assert_eq!(
        h.transactions.rollover_count(all[0].id),
        1,
        "exactly one rollover despite 3 inits"
    );
}

#[tokio::test]
async fn multi_month_catch_up_is_idempotent_on_replay() {
    let h = harness();
    h.service
        .ensure_current_month(h.user_id, ny_noon(2026, 1, 5))
        .await
        .expect("init jan");
    // Catch up to April, then replay the same catch-up.
    h.service
        .ensure_current_month(h.user_id, ny_noon(2026, 4, 5))
        .await
        .expect("catch up");
    h.service
        .ensure_current_month(h.user_id, ny_noon(2026, 4, 5))
        .await
        .expect("replay catch up");
    let all = h.months.list_for_user(h.user_id).await.expect("list");
    assert_eq!(all.len(), 4);
    for m in &all {
        assert_eq!(h.transactions.rollover_count(m.id), 1);
    }
}

// ---------------------------------------------------------------------------
// Rolling-Other chain: cent conservation across a multi-month chain
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rolling_chain_conserves_cents_with_income_variance() {
    // Build a Jan with an awkward income surplus + expenses, advance to Feb, and
    // assert the Feb rollover equals the exact D5 net to the cent.
    let h = harness();
    let exp_cat = add_expense_category(&h, "Groceries");

    let jan = h
        .service
        .ensure_current_month(h.user_id, ny_noon(2026, 1, 8))
        .await
        .expect("init jan");

    // Expected income is $4,000 (semimonthly $2,000 x 2). Actual = $4,133.37.
    let mut income = base_txn(&h, jan.id, Money::from_minor(413_337));
    income.income_kind = Some(IncomeKind::Budgeted);
    push_txn(&h, income);

    // Two awkward expenses.
    let mut e1 = base_txn(&h, jan.id, Money::from_minor(-12_399));
    e1.category_id = Some(exp_cat);
    push_txn(&h, e1);
    let mut e2 = base_txn(&h, jan.id, Money::from_minor(-7_801));
    e2.category_id = Some(exp_cat);
    push_txn(&h, e2);

    h.service
        .ensure_current_month(h.user_id, ny_noon(2026, 2, 8))
        .await
        .expect("init feb");

    let feb = h
        .months
        .find_by_year_month(h.user_id, 2026, 2)
        .await
        .expect("lookup")
        .expect("feb");
    let rollover = h
        .transactions
        .find_rollover_for_month(feb.id)
        .await
        .expect("lookup")
        .expect("rollover");

    // D5 oracle: (413337 - 400000) + (-12399 - 7801) cents = 13337 - 20200 = -6863.
    let expected = net_leftover(
        Money::from_minor(413_337),
        Money::from_minor(400_000),
        Money::from_minor(-12_399 - 7_801),
    );
    assert_eq!(expected, Money::from_minor(-6_863));
    assert_eq!(
        rollover.amount, expected,
        "Feb rollover matches D5 to the cent"
    );
}

// ---------------------------------------------------------------------------
// Pending exclusion (BUDGET-STATUS-DRIVES-INCLUSION-1)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pending_transactions_are_excluded_from_the_rollover_net() {
    let h = harness();
    let exp_cat = add_expense_category(&h, "Groceries");

    let jan = h
        .service
        .ensure_current_month(h.user_id, ny_noon(2026, 1, 8))
        .await
        .expect("init jan");

    // Income meets expectation exactly.
    let mut income = base_txn(&h, jan.id, Money::from_major(4_000));
    income.income_kind = Some(IncomeKind::Budgeted);
    push_txn(&h, income);

    // A settled -$50 expense (counts) and a pending -$500 expense (excluded).
    let mut settled = base_txn(&h, jan.id, Money::from_major(-50));
    settled.category_id = Some(exp_cat);
    push_txn(&h, settled);

    let mut pending = base_txn(&h, jan.id, Money::from_major(-500));
    pending.category_id = Some(exp_cat);
    pending.status = TransactionStatus::Pending;
    push_txn(&h, pending);

    h.service
        .ensure_current_month(h.user_id, ny_noon(2026, 2, 8))
        .await
        .expect("init feb");

    let feb = h
        .months
        .find_by_year_month(h.user_id, 2026, 2)
        .await
        .expect("lookup")
        .expect("feb");
    let rollover = h
        .transactions
        .find_rollover_for_month(feb.id)
        .await
        .expect("lookup")
        .expect("rollover");
    assert_eq!(
        rollover.amount,
        Money::from_major(-50),
        "pending excluded: only the settled -$50 nets"
    );
}

// ---------------------------------------------------------------------------
// Timezone month-membership (D2 / ARCH-UTC-TIMESTAMPS-1)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn membership_uses_home_tz_not_utc() {
    // The discriminating case: an instant where UTC and the home TZ disagree on
    // the month. 2026-02-28 23:00 America/New_York is 2026-03-01 04:00 UTC. A
    // naive UTC calculation would mis-attribute this to MARCH; membership in the
    // home TZ (D2) correctly resolves it to FEBRUARY.
    let h = harness();
    let instant = ny_local(2026, 2, 28, 23);
    // Sanity-check the fixture really does straddle the boundary in UTC.
    assert_eq!(
        (instant.year(), instant.month(), instant.day()),
        (2026, 3, 1),
        "fixture must be March 1 in UTC so the test actually exercises the TZ split"
    );

    let m = h
        .service
        .ensure_current_month(h.user_id, instant)
        .await
        .expect("init");
    assert_eq!(
        (m.year, m.month),
        (2026, 2),
        "membership resolves in America/New_York, not UTC"
    );
}
