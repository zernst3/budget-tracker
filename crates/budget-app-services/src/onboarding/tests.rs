//! Unit tests for the onboarding / initial-load service (`SPEC §4.6`, `§12` D8).
//!
//! DB-free in-memory fakes (a `Mutex`-wrapped store per aggregate), mirroring the
//! month-lifecycle / fund test style. The transaction + fund fakes upsert by id
//! (replace the slot when the id matches), exactly reproducing the production
//! upsert (ON CONFLICT (pk) DO UPDATE) that makes the deterministically-keyed
//! opening rows idempotent (`BUDGET-CUTOVER-1`). The tests assert:
//!   - one settled opening charge per non-zero category, dated `tracking_start_date`
//!     (the genesis boundary, never before it — `BUDGET-CUTOVER-1`);
//!   - a $0 category gets NO opening charge (`SPEC §4.6`);
//!   - the starting rolling-Other balance posts ONE line on the genesis month's
//!     rollover bucket (`BUDGET-FUND-EARMARK-1` / D6 Model A);
//!   - the starting buffer-fund balance is SET on the fund, not double-posted as an
//!     Other expense (`BUDGET-FUND-EARMARK-1`);
//!   - re-running the seed is idempotent (identical state, no duplicates / no
//!     double-counted balances) and re-seeding new figures upserts coherently
//!     (`SPEC §12` onboarding path); and
//!   - clean month-start coherence: the FIRST step-4 rollover OUT of the genesis
//!     month computes a correct prior-month net over the opening positions
//!     (`BUDGET-ROLLOVER-INTEGRITY-1`).
//!
//! ### Lint suppressions (test-only)
//!
//! The workspace denies `unwrap_used`, `expect_used`, and `panic` in production
//! code; tests intentionally use them, suppressed for this module only.

#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]
#![allow(clippy::panic)]

use std::any::Any;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::{NaiveDate, Utc};

use budget_domain::budget::Budget;
use budget_domain::category::Category;
use budget_domain::enums::{Cadence, CategoryGrp, FundKind, TransactionSource, TransactionStatus};
use budget_domain::fund::Fund;
use budget_domain::ids::{
    BudgetId, CategoryId, CategoryKey, FundId, MonthId, RepaymentObligationId, TransactionId,
    UserId,
};
use budget_domain::money::Money;
use budget_domain::month::Month;
use budget_domain::predicates::counts_in_budget;
use budget_domain::repayment_obligation::RepaymentObligation;
use budget_domain::repositories::{
    BudgetRepository, FundRepository, MonthRepository, TransactionRepository, UserRepository,
};
use budget_domain::transaction::Transaction;
use budget_domain::uow::{UnitOfWork, UowFuture, UowProvider};
use budget_domain::user::User;
use budget_domain::validated::Email;
use budget_domain::{CategorySpent, MonthNet, RepositoryError};

use crate::income::FixedExpectation;
use crate::month_lifecycle::MonthLifecycleService;

use super::*;

// ---------------------------------------------------------------------------
// Fakes (DB-free, upsert-by-id so deterministic opening ids are idempotent)
// ---------------------------------------------------------------------------

struct FakeUow;
impl UnitOfWork for FakeUow {
    fn as_any(&self) -> &dyn Any {
        self
    }
}

type BoxedUowClosure<'a> =
    Box<dyn for<'u> FnOnce(&'u dyn UnitOfWork) -> UowFuture<'u, Box<dyn Any + Send>> + Send + 'a>;

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

fn poisoned<T>(_e: std::sync::PoisonError<T>) -> RepositoryError {
    RepositoryError::Database("test mutex poisoned".to_owned())
}

#[derive(Default)]
struct UserStore {
    users: Vec<User>,
}
struct FakeUserRepo {
    store: Mutex<UserStore>,
}
impl FakeUserRepo {
    fn new() -> Self {
        Self {
            store: Mutex::new(UserStore::default()),
        }
    }
}
#[async_trait]
impl UserRepository for FakeUserRepo {
    async fn find_by_id(&self, id: UserId) -> Result<Option<User>, RepositoryError> {
        let store = self.store.lock().map_err(poisoned)?;
        Ok(store.users.iter().find(|u| u.id == id).cloned())
    }
    async fn find_by_email(&self, email: &str) -> Result<Option<User>, RepositoryError> {
        let store = self.store.lock().map_err(poisoned)?;
        Ok(store
            .users
            .iter()
            .find(|u| u.email.as_str() == email)
            .cloned())
    }
    async fn save(
        &self,
        user: &User,
        _uow: Option<&dyn UnitOfWork>,
    ) -> Result<(), RepositoryError> {
        let mut store = self.store.lock().map_err(poisoned)?;
        if let Some(slot) = store.users.iter_mut().find(|u| u.id == user.id) {
            *slot = user.clone();
        } else {
            store.users.push(user.clone());
        }
        Ok(())
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
            .max_by_key(|m| (m.year, m.month))
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
        v.sort_by_key(|m| (m.year, m.month));
        Ok(v)
    }
    async fn create_if_absent(
        &self,
        month: &Month,
        _uow: Option<&dyn UnitOfWork>,
    ) -> Result<Month, RepositoryError> {
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
    fn all(&self) -> Vec<Transaction> {
        self.store.lock().unwrap().txns.clone()
    }
    fn count(&self) -> usize {
        self.store.lock().unwrap().txns.len()
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
    fn balance(&self, fund_id: FundId) -> Option<Money> {
        self.store
            .lock()
            .unwrap()
            .funds
            .iter()
            .find(|f| f.id == fund_id)
            .map(|f| f.balance)
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
            .find(|o| o.transaction_id == transaction_id)
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
            .map(|o| o.transaction_id)
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

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn ymd(y: i32, m: u32, d: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, d).unwrap()
}

fn money(units: i64) -> Money {
    Money::from_decimal(rust_decimal::Decimal::new(units * 100, 2))
}

struct Harness {
    users: Arc<FakeUserRepo>,
    budgets: Arc<FakeBudgetRepo>,
    months: Arc<FakeMonthRepo>,
    transactions: Arc<FakeTransactionRepo>,
    funds: Arc<FakeFundRepo>,
    service: OnboardingService,
    user_id: UserId,
    budget_id: BudgetId,
    rollover_bucket_id: CategoryId,
    groceries_id: CategoryId,
    utilities_id: CategoryId,
    buffer_fund_id: FundId,
}

/// Build a harness with a user (`tracking_start_date` set by the caller), one
/// open-ended budget version with a rollover bucket + two ordinary categories, and
/// one buffer fund with a $0 starting balance.
fn harness(tracking_start: NaiveDate) -> Harness {
    let users = Arc::new(FakeUserRepo::new());
    let budgets = Arc::new(FakeBudgetRepo::new());
    let months = Arc::new(FakeMonthRepo::new());
    let transactions = Arc::new(FakeTransactionRepo::new());
    let funds = Arc::new(FakeFundRepo::new());

    let user_id = UserId::generate();
    let budget_id = BudgetId::generate();
    let rollover_bucket_id = CategoryId::generate();
    let groceries_id = CategoryId::generate();
    let utilities_id = CategoryId::generate();
    let buffer_fund_id = FundId::generate();

    {
        let mut us = users.store.lock().unwrap();
        us.users.push(User {
            id: user_id,
            email: Email::try_new("zach@example.com").unwrap(),
            password_hash: "x".to_owned(),
            totp_secret: None,
            tracking_start_date: tracking_start,
            created_at: Utc::now(),
        });
    }
    {
        let mut bs = budgets.store.lock().unwrap();
        bs.budgets.push(Budget {
            id: budget_id,
            user_id,
            name: "Test Budget".to_owned(),
            effective_from: ymd(2020, 1, 1),
            effective_to: None,
            created_at: Utc::now(),
        });
        let mk = |id: CategoryId, name: &str, is_rollover: bool, grp: CategoryGrp| Category {
            id,
            budget_id,
            category_key: CategoryKey::generate(),
            name: name.to_owned(),
            amount: Money::ZERO,
            grp,
            settle_type: None,
            expected_bills: None,
            is_rollover_bucket: is_rollover,
            cadence: Cadence::Monthly,
            period_months: None,
            fund_balance: Money::ZERO,
            next_due_date: None,
            sort_order: 0,
        };
        bs.categories.push(mk(
            rollover_bucket_id,
            "Other",
            true,
            CategoryGrp::Discretionary,
        ));
        bs.categories.push(mk(
            groceries_id,
            "Groceries",
            false,
            CategoryGrp::Discretionary,
        ));
        bs.categories
            .push(mk(utilities_id, "Utilities", false, CategoryGrp::Fixed));
    }
    {
        let mut fs = funds.store.lock().unwrap();
        fs.funds.push(Fund {
            id: buffer_fund_id,
            user_id,
            name: "Buffer".to_owned(),
            kind: FundKind::Buffer,
            balance: Money::ZERO,
            target_balance: Some(money(5000)),
            compulsory_repayment: true,
            created_at: Utc::now(),
        });
    }

    let service = OnboardingService::new(
        Arc::clone(&users) as Arc<dyn UserRepository>,
        Arc::clone(&budgets) as Arc<dyn BudgetRepository>,
        Arc::clone(&months) as Arc<dyn MonthRepository>,
        Arc::clone(&transactions) as Arc<dyn TransactionRepository>,
        Arc::clone(&funds) as Arc<dyn FundRepository>,
        Arc::new(FakeUowProvider),
    );

    Harness {
        users,
        budgets,
        months,
        transactions,
        funds,
        service,
        user_id,
        budget_id,
        rollover_bucket_id,
        groceries_id,
        utilities_id,
        buffer_fund_id,
    }
}

fn base_input(h: &Harness) -> OnboardingInput {
    OnboardingInput {
        user_id: h.user_id,
        category_charges: vec![
            CategoryOpeningCharge {
                category_id: h.groceries_id,
                spend_so_far: money(300),
            },
            // A $0 category: must get NO opening charge (SPEC §4.6).
            CategoryOpeningCharge {
                category_id: h.utilities_id,
                spend_so_far: Money::ZERO,
            },
        ],
        starting_other_balance: money(212),
        starting_buffer: Some(BufferOpeningBalance {
            fund_id: h.buffer_fund_id,
            balance: money(5000),
        }),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Opening charges: one settled manual expense per non-zero category, dated the
/// genesis boundary, magnitude = month-to-date spend; $0 category skipped.
#[tokio::test]
async fn seeds_per_category_opening_charges_dated_genesis_skipping_zero() {
    let genesis = ymd(2026, 7, 1);
    let h = harness(genesis);
    let report = h.service.seed(&base_input(&h), Utc::now()).await.unwrap();

    assert_eq!(
        report.opening_charges_posted, 1,
        "only the non-zero category"
    );
    assert_eq!(report.genesis_date, genesis);

    let charge_id = opening_charge_id(h.user_id, h.groceries_id);
    let charge = h
        .transactions
        .all()
        .into_iter()
        .find(|t| t.id == charge_id)
        .expect("groceries opening charge present");
    assert_eq!(charge.amount, -money(300), "expense = negative magnitude");
    assert_eq!(
        charge.date, genesis,
        "dated the genesis boundary, never before"
    );
    assert_eq!(charge.status, TransactionStatus::Settled);
    assert_eq!(charge.source, TransactionSource::Manual);
    assert!(!charge.is_rollover && !charge.is_fund_draw);

    // The $0 utilities category got NO row.
    let util_id = opening_charge_id(h.user_id, h.utilities_id);
    assert!(
        h.transactions.all().iter().all(|t| t.id != util_id),
        "a $0 category gets no opening charge (SPEC §4.6)"
    );
}

/// BUDGET-CUTOVER-1: NO opening transaction is dated before `tracking_start_date`.
#[tokio::test]
async fn no_opening_row_is_dated_before_tracking_start_date() {
    let genesis = ymd(2026, 7, 15); // mid-month day-1
    let h = harness(genesis);
    h.service.seed(&base_input(&h), Utc::now()).await.unwrap();
    for t in h.transactions.all() {
        assert!(
            t.date >= genesis,
            "no row may predate the genesis boundary (BUDGET-CUTOVER-1); got {}",
            t.date
        );
    }
}

/// The starting rolling-Other balance posts ONE line on the genesis month's
/// rollover bucket with the signed balance verbatim (D6 Model A).
#[tokio::test]
async fn seeds_starting_other_line_on_rollover_bucket() {
    let h = harness(ymd(2026, 7, 1));
    let report = h.service.seed(&base_input(&h), Utc::now()).await.unwrap();
    assert!(report.other_line_posted);

    let other = h
        .transactions
        .all()
        .into_iter()
        .find(|t| t.id == opening_other_id(h.user_id))
        .expect("opening-other line present");
    assert_eq!(other.amount, money(212), "signed starting balance verbatim");
    assert_eq!(other.category_id, Some(h.rollover_bucket_id));
    assert!(
        !other.is_rollover,
        "onboarding opening line, not a system rollover"
    );
}

/// The starting buffer balance is SET on the fund, and NOT double-posted as an
/// Other-bucket contribution (BUDGET-FUND-EARMARK-1: counted once).
#[tokio::test]
async fn seeds_buffer_balance_without_double_posting() {
    let h = harness(ymd(2026, 7, 1));
    let report = h.service.seed(&base_input(&h), Utc::now()).await.unwrap();
    assert!(report.buffer_seeded);

    assert_eq!(h.funds.balance(h.buffer_fund_id), Some(money(5000)));

    // Exactly two transactions exist (1 opening charge + 1 opening Other). The
    // buffer is NOT a third (no contribution expense).
    assert_eq!(
        h.transactions.count(),
        2,
        "buffer balance is a fund fact, not an Other expense (BUDGET-FUND-EARMARK-1)"
    );
}

/// Re-running the seed with the same input is idempotent: identical state, no
/// duplicated charges, no double-counted balance (BUDGET-CUTOVER-1, re-runnable).
#[tokio::test]
async fn re_running_is_idempotent() {
    let h = harness(ymd(2026, 7, 1));
    let input = base_input(&h);

    h.service.seed(&input, Utc::now()).await.unwrap();
    let after_first = h.transactions.all();
    let first_count = after_first.len();
    let first_buffer = h.funds.balance(h.buffer_fund_id);

    h.service.seed(&input, Utc::now()).await.unwrap();
    let after_second = h.transactions.all();

    assert_eq!(
        after_second.len(),
        first_count,
        "re-run creates no duplicate opening rows"
    );
    assert_eq!(
        h.funds.balance(h.buffer_fund_id),
        first_buffer,
        "buffer is SET (idempotent), never accumulated"
    );
    // The opening Other line is a single row with the same amount, not stacked.
    let others: Vec<_> = after_second
        .iter()
        .filter(|t| t.id == opening_other_id(h.user_id))
        .collect();
    assert_eq!(others.len(), 1);
    assert_eq!(others[0].amount, money(212));
}

/// SPEC §12 onboarding path: re-seeding to new figures upserts the
/// deterministically-keyed rows coherently (test phase -> clean reset).
#[tokio::test]
async fn re_seeding_new_figures_replaces_coherently() {
    let h = harness(ymd(2026, 7, 1));
    h.service.seed(&base_input(&h), Utc::now()).await.unwrap();

    let revised = OnboardingInput {
        user_id: h.user_id,
        category_charges: vec![CategoryOpeningCharge {
            category_id: h.groceries_id,
            spend_so_far: money(450), // revised figure
        }],
        starting_other_balance: money(999),
        starting_buffer: Some(BufferOpeningBalance {
            fund_id: h.buffer_fund_id,
            balance: money(6000),
        }),
    };
    h.service.seed(&revised, Utc::now()).await.unwrap();

    let charge = h
        .transactions
        .all()
        .into_iter()
        .find(|t| t.id == opening_charge_id(h.user_id, h.groceries_id))
        .unwrap();
    assert_eq!(charge.amount, -money(450), "upserted to the revised figure");
    let other = h
        .transactions
        .all()
        .into_iter()
        .find(|t| t.id == opening_other_id(h.user_id))
        .unwrap();
    assert_eq!(other.amount, money(999));
    assert_eq!(h.funds.balance(h.buffer_fund_id), Some(money(6000)));
}

/// Clean month-start coherence (point 6): the FIRST step-4 rollover OUT of the
/// genesis month computes a correct prior-month net over the opening positions.
///
/// Genesis = 2026-07-01. Opening: Other +$212, groceries −$300. The genesis-month
/// net = +212 − 300 = −$88. After onboarding, running the month lifecycle into
/// August must post an August rollover of −$88.
#[tokio::test]
async fn clean_month_start_first_rollover_is_correct() {
    let h = harness(ymd(2026, 7, 1));
    let input = OnboardingInput {
        user_id: h.user_id,
        category_charges: vec![CategoryOpeningCharge {
            category_id: h.groceries_id,
            spend_so_far: money(300),
        }],
        starting_other_balance: money(212),
        starting_buffer: None,
    };
    h.service.seed(&input, Utc::now()).await.unwrap();

    // Drive lazy-init from genesis (July) to August. Zero expected income so the
    // income variance term is the actual income (here: none) → the net is purely
    // the opening expense-remaining sum.
    let lifecycle = MonthLifecycleService::new(
        Arc::clone(&h.months) as Arc<dyn MonthRepository>,
        Arc::clone(&h.budgets) as Arc<dyn BudgetRepository>,
        Arc::clone(&h.transactions) as Arc<dyn TransactionRepository>,
        Arc::clone(&h.funds) as Arc<dyn FundRepository>,
        Arc::new(FakeUowProvider),
        Arc::new(FixedExpectation::zero()),
    );
    // "now" in August 2026 (home tz) so lazy-init creates August and posts its
    // rollover from July.
    let now = chrono::TimeZone::with_ymd_and_hms(&Utc, 2026, 8, 5, 12, 0, 0).unwrap();
    let august = lifecycle
        .ensure_current_month(h.user_id, now)
        .await
        .unwrap();
    assert_eq!((august.year, august.month), (2026, 8));

    let aug_rollover = h
        .transactions
        .all()
        .into_iter()
        .find(|t| t.month_id == august.id && t.is_rollover)
        .expect("August rollover posted");
    assert_eq!(
        aug_rollover.amount,
        -money(88),
        "rollover = genesis-month net = +212 (Other) − 300 (groceries)"
    );
}

/// A genesis month that does not yet exist is created (no rollover posted by
/// onboarding — the starting-Other line is the carryover, BUDGET-CUTOVER-1).
#[tokio::test]
async fn creates_genesis_month_without_a_rollover() {
    let h = harness(ymd(2026, 7, 1));
    h.service.seed(&base_input(&h), Utc::now()).await.unwrap();

    let genesis = h
        .months
        .find_by_year_month(h.user_id, 2026, 7)
        .await
        .unwrap()
        .expect("genesis month created");
    let rollover = h
        .transactions
        .all()
        .into_iter()
        .find(|t| t.month_id == genesis.id && t.is_rollover);
    assert!(
        rollover.is_none(),
        "onboarding posts NO system rollover into the genesis month"
    );
    // Quiet unused-field warnings on harness fields kept for symmetry.
    let _ = (&h.users, &h.budget_id);
}
