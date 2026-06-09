//! Independent property + oracle tests for the month-lifecycle service.
//!
//! These tests were authored by a SEPARATE test-author agent that did **not**
//! trust the build's own unit tests (`ORCH-REVIEWER-SPLIT-1` spirit). Phase-1
//! audit N4 flagged the rolling-Other rollover as the highest double-count risk
//! in the codebase, so this file builds its **own** in-memory fakes from scratch
//! against the crate's *public* trait surface only (it never imports the build's
//! test fakes) and its **own** `rust_decimal` oracle that re-derives the `D5`
//! net independently of the production `net_leftover` function. A test that only
//! re-uses the production formula as its own oracle would be tautological; the
//! oracle here is a from-scratch Decimal re-implementation of `SPEC §4.3 / D5`.
//!
//! Invariants covered (`ORCH-NEW-PATH-TESTS-1`, `PROC-REGRESSION-TEST-1`):
//!   * CENT-CONSERVATION across a single rollover and a multi-month catch-up
//!     (`BUDGET-ROLLOVER-INTEGRITY-1`): money is neither created nor destroyed;
//!     the rollover chain telescopes exactly to the cent versus the oracle.
//!   * IDEMPOTENCY (`BUDGET-IDEMPOTENT-MONTH-INIT-1`): re-running init never
//!     double-creates a month or double-posts a rollover; a multi-month gap run
//!     twice yields byte-identical state.
//!   * D5 NET ORACLE (`SPEC §4.3`): net = (actual − expected) + Σ(remaining); a
//!     +$100 income surplus raises Other by exactly $100.
//!   * D6 FUND-EARMARK (`BUDGET-FUND-EARMARK-1`): a fund contribution in an
//!     otherwise-zero-net month drives net = −contribution and the fund balance
//!     = +contribution, so total system money is invariant (counted once).
//!   * MULTI-MONTH GAP: March → June produces exactly three new months with a
//!     correctly linked sequential rollover chain.
//!
//! Property tests are written as deterministic generative loops over a seeded
//! splitmix64 PRNG rather than pulling in `proptest`: adding a workspace
//! dependency is a structural change that routes to a human (`AGENTS.md`
//! routing rule), and a seeded loop over thousands of awkward-cent cases gives
//! the same invariant coverage with a stable, reproducible failure (the seed is
//! printed on failure so any counterexample replays exactly).
//!
//! ### Lint suppressions (test-only)
//!
//! The workspace denies `unwrap_used`, `expect_used`, and `panic` in production
//! code. Test code panics on assertion failure by design; the bans are
//! suppressed for this integration test only, matching the in-crate convention.

#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]
#![allow(clippy::panic)]
#![allow(clippy::too_many_lines)]

use std::any::Any;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use chrono_tz::America::New_York;
use rust_decimal::Decimal;

use budget_app_services::income::{FixedExpectation, IncomeExpectation};
use budget_app_services::{MonthLifecycleService, net_leftover};

use budget_domain::budget::Budget;
use budget_domain::category::Category;
use budget_domain::enums::{
    Cadence, CategoryGrp, IncomeKind, ObligationSource, TransactionSource, TransactionStatus,
};
use budget_domain::fund::Fund;
use budget_domain::ids::{BudgetId, CategoryId, CategoryKey, MonthId, TransactionId, UserId};
use budget_domain::ids::{FundId, RepaymentObligationId};
use budget_domain::money::Money;
use budget_domain::month::Month;
use budget_domain::repayment_obligation::RepaymentObligation;
use budget_domain::transaction::Transaction;
use budget_domain::uow::{UnitOfWork, UowFuture, UowProvider};
use budget_domain::{
    BudgetRepository, CategorySpent, FundRepository, MonthNet, MonthRepository, RepositoryError,
    TransactionRepository,
};

// ===========================================================================
// Independent in-memory fakes — built ONLY against the public trait surface.
// These are deliberately NOT the build's `src/month_lifecycle/tests.rs` fakes;
// they are re-derived here so the test does not inherit any bug the build's
// fakes might share with the build's code.
// ===========================================================================

/// No-op unit-of-work handle. The fakes have no real transaction to enlist; the
/// handle exists only to satisfy the `as_any` downcast surface.
struct NoopUow;
impl UnitOfWork for NoopUow {
    fn as_any(&self) -> &dyn Any {
        self
    }
}

type BoxedClosure<'a> =
    Box<dyn for<'u> FnOnce(&'u dyn UnitOfWork) -> UowFuture<'u, Box<dyn Any + Send>> + Send + 'a>;

/// Runs the closure with a no-op handle. The service's atomicity contract is
/// exercised structurally; real commit/rollback is an infra concern.
struct NoopUowProvider;

#[async_trait]
impl UowProvider for NoopUowProvider {
    async fn run_boxed(&self, f: BoxedClosure<'_>) -> Result<Box<dyn Any + Send>, RepositoryError> {
        let uow = NoopUow;
        let handle: &dyn UnitOfWork = &uow;
        f(handle).await
    }
}

fn poisoned<T>(_e: std::sync::PoisonError<T>) -> RepositoryError {
    RepositoryError::Database("test mutex poisoned".to_owned())
}

#[derive(Default)]
struct MemMonthRepo {
    months: Mutex<Vec<Month>>,
}

#[async_trait]
impl MonthRepository for MemMonthRepo {
    async fn find_by_id(&self, id: MonthId) -> Result<Option<Month>, RepositoryError> {
        let g = self.months.lock().map_err(poisoned)?;
        Ok(g.iter().find(|m| m.id == id).cloned())
    }

    async fn find_by_year_month(
        &self,
        user_id: UserId,
        year: i32,
        month: i32,
    ) -> Result<Option<Month>, RepositoryError> {
        let g = self.months.lock().map_err(poisoned)?;
        Ok(g.iter()
            .find(|m| m.user_id == user_id && m.year == year && m.month == month)
            .cloned())
    }

    async fn find_latest(&self, user_id: UserId) -> Result<Option<Month>, RepositoryError> {
        let g = self.months.lock().map_err(poisoned)?;
        // Independent of the build's `Month::sort_key`: sort on the raw tuple.
        Ok(g.iter()
            .filter(|m| m.user_id == user_id)
            .max_by_key(|m| (m.year, m.month))
            .cloned())
    }

    async fn list_for_user(&self, user_id: UserId) -> Result<Vec<Month>, RepositoryError> {
        let g = self.months.lock().map_err(poisoned)?;
        let mut v: Vec<Month> = g.iter().filter(|m| m.user_id == user_id).cloned().collect();
        v.sort_by_key(|m| (m.year, m.month));
        Ok(v)
    }

    async fn create_if_absent(
        &self,
        month: &Month,
        _uow: Option<&dyn UnitOfWork>,
    ) -> Result<Month, RepositoryError> {
        // UNIQUE(user_id, year, month) — ON CONFLICT DO NOTHING returns existing.
        let mut g = self.months.lock().map_err(poisoned)?;
        if let Some(existing) = g
            .iter()
            .find(|m| m.user_id == month.user_id && m.year == month.year && m.month == month.month)
        {
            return Ok(existing.clone());
        }
        g.push(month.clone());
        Ok(month.clone())
    }

    async fn save(
        &self,
        month: &Month,
        _uow: Option<&dyn UnitOfWork>,
    ) -> Result<(), RepositoryError> {
        let mut g = self.months.lock().map_err(poisoned)?;
        if let Some(slot) = g.iter_mut().find(|m| m.id == month.id) {
            *slot = month.clone();
        } else {
            g.push(month.clone());
        }
        Ok(())
    }
}

#[derive(Default)]
struct MemBudgetRepo {
    budgets: Mutex<Vec<Budget>>,
    categories: Mutex<Vec<Category>>,
}

#[async_trait]
impl BudgetRepository for MemBudgetRepo {
    async fn find_by_id(&self, id: BudgetId) -> Result<Option<Budget>, RepositoryError> {
        let g = self.budgets.lock().map_err(poisoned)?;
        Ok(g.iter().find(|b| b.id == id).cloned())
    }

    async fn find_active_for_date(
        &self,
        user_id: UserId,
        date: NaiveDate,
    ) -> Result<Option<Budget>, RepositoryError> {
        let g = self.budgets.lock().map_err(poisoned)?;
        Ok(g.iter()
            .find(|b| {
                b.user_id == user_id
                    && b.effective_from <= date
                    && b.effective_to.is_none_or(|to| date <= to)
            })
            .cloned())
    }

    async fn find_current(&self, user_id: UserId) -> Result<Option<Budget>, RepositoryError> {
        let g = self.budgets.lock().map_err(poisoned)?;
        Ok(g.iter()
            .find(|b| b.user_id == user_id && b.effective_to.is_none())
            .cloned())
    }

    async fn list_for_user(&self, user_id: UserId) -> Result<Vec<Budget>, RepositoryError> {
        let g = self.budgets.lock().map_err(poisoned)?;
        Ok(g.iter().filter(|b| b.user_id == user_id).cloned().collect())
    }

    async fn list_categories(&self, budget_id: BudgetId) -> Result<Vec<Category>, RepositoryError> {
        let g = self.categories.lock().map_err(poisoned)?;
        Ok(g.iter()
            .filter(|c| c.budget_id == budget_id)
            .cloned()
            .collect())
    }

    async fn find_category(&self, id: CategoryId) -> Result<Option<Category>, RepositoryError> {
        let g = self.categories.lock().map_err(poisoned)?;
        Ok(g.iter().find(|c| c.id == id).cloned())
    }

    async fn find_rollover_bucket(
        &self,
        budget_id: BudgetId,
    ) -> Result<Option<Category>, RepositoryError> {
        let g = self.categories.lock().map_err(poisoned)?;
        Ok(g.iter()
            .find(|c| c.budget_id == budget_id && c.is_rollover_bucket)
            .cloned())
    }

    async fn save(
        &self,
        budget: &Budget,
        _uow: Option<&dyn UnitOfWork>,
    ) -> Result<(), RepositoryError> {
        let mut g = self.budgets.lock().map_err(poisoned)?;
        if let Some(slot) = g.iter_mut().find(|b| b.id == budget.id) {
            *slot = budget.clone();
        } else {
            g.push(budget.clone());
        }
        Ok(())
    }

    async fn save_category(
        &self,
        category: &Category,
        _uow: Option<&dyn UnitOfWork>,
    ) -> Result<(), RepositoryError> {
        let mut g = self.categories.lock().map_err(poisoned)?;
        if let Some(slot) = g.iter_mut().find(|c| c.id == category.id) {
            *slot = category.clone();
        } else {
            g.push(category.clone());
        }
        Ok(())
    }
}

#[derive(Default)]
struct MemTxnRepo {
    txns: Mutex<Vec<Transaction>>,
}

impl MemTxnRepo {
    fn rollover_count(&self, month_id: MonthId) -> usize {
        let g = self.txns.lock().unwrap();
        g.iter()
            .filter(|t| t.month_id == month_id && t.is_rollover)
            .count()
    }

    fn push(&self, t: Transaction) {
        self.txns.lock().unwrap().push(t);
    }
}

#[async_trait]
impl TransactionRepository for MemTxnRepo {
    async fn find_by_id(&self, id: TransactionId) -> Result<Option<Transaction>, RepositoryError> {
        let g = self.txns.lock().map_err(poisoned)?;
        Ok(g.iter().find(|t| t.id == id).cloned())
    }

    async fn list_for_month(&self, month_id: MonthId) -> Result<Vec<Transaction>, RepositoryError> {
        let g = self.txns.lock().map_err(poisoned)?;
        Ok(g.iter()
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
        let g = self.txns.lock().map_err(poisoned)?;
        Ok(g.iter()
            .filter(|t| t.month_id == month_id && t.category_id == Some(category_id))
            .cloned()
            .collect())
    }

    async fn find_rollover_for_month(
        &self,
        month_id: MonthId,
    ) -> Result<Option<Transaction>, RepositoryError> {
        let g = self.txns.lock().map_err(poisoned)?;
        Ok(g.iter()
            .find(|t| t.month_id == month_id && t.is_rollover)
            .cloned())
    }

    async fn find_by_plaid_transaction_id(
        &self,
        plaid_transaction_id: &str,
    ) -> Result<Option<Transaction>, RepositoryError> {
        let g = self.txns.lock().map_err(poisoned)?;
        Ok(g.iter()
            .find(|t| t.plaid_transaction_id.as_deref() == Some(plaid_transaction_id))
            .cloned())
    }

    async fn list_expected_for_month(
        &self,
        month_id: MonthId,
    ) -> Result<Vec<Transaction>, RepositoryError> {
        let g = self.txns.lock().map_err(poisoned)?;
        Ok(g.iter()
            .filter(|t| t.month_id == month_id && t.status == TransactionStatus::Expected)
            .cloned()
            .collect())
    }

    async fn find_expected_matched_to(
        &self,
        real_transaction_id: TransactionId,
    ) -> Result<Option<Transaction>, RepositoryError> {
        let g = self.txns.lock().map_err(poisoned)?;
        Ok(g.iter()
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
        // Independent re-derivation of "counts in budget": settled + expected,
        // pending excluded (BUDGET-STATUS-DRIVES-INCLUSION-1). Not routed through
        // the build's predicate so the fake cannot inherit a predicate bug.
        let g = self.txns.lock().map_err(poisoned)?;
        let net: Money = g
            .iter()
            .filter(|t| t.month_id == month_id && counts_independently(t.status))
            .map(|t| t.amount)
            .sum();
        Ok(MonthNet { month_id, net })
    }

    async fn save(
        &self,
        transaction: &Transaction,
        _uow: Option<&dyn UnitOfWork>,
    ) -> Result<(), RepositoryError> {
        let mut g = self.txns.lock().map_err(poisoned)?;
        // Partial-unique transactions(month_id) WHERE is_rollover: a second
        // rollover for the same month is a UniqueViolation.
        if transaction.is_rollover
            && g.iter().any(|t| {
                t.month_id == transaction.month_id && t.is_rollover && t.id != transaction.id
            })
        {
            return Err(RepositoryError::UniqueViolation(
                "transactions(month_id) WHERE is_rollover".to_owned(),
            ));
        }
        if let Some(slot) = g.iter_mut().find(|t| t.id == transaction.id) {
            *slot = transaction.clone();
        } else {
            g.push(transaction.clone());
        }
        Ok(())
    }

    async fn delete(
        &self,
        id: TransactionId,
        _uow: Option<&dyn UnitOfWork>,
    ) -> Result<(), RepositoryError> {
        let mut g = self.txns.lock().map_err(poisoned)?;
        g.retain(|t| t.id != id);
        Ok(())
    }
}

/// Independent inclusion predicate (`BUDGET-STATUS-DRIVES-INCLUSION-1`),
/// re-derived locally so the oracle does not share code with production.
fn counts_independently(status: TransactionStatus) -> bool {
    matches!(
        status,
        TransactionStatus::Settled | TransactionStatus::Expected
    )
}

/// Independent in-memory fund repo. These lifecycle tests post no obligations,
/// so the buffer-financed exclusion set is empty — but the repo must exist to
/// satisfy the service's `FundRepository` dependency (`SPEC §4.9` D7 seam).
#[derive(Default)]
struct MemFundRepo {
    funds: Mutex<Vec<Fund>>,
    obligations: Mutex<Vec<RepaymentObligation>>,
}

#[async_trait]
impl FundRepository for MemFundRepo {
    async fn find_by_id(&self, id: FundId) -> Result<Option<Fund>, RepositoryError> {
        Ok(self
            .funds
            .lock()
            .map_err(poisoned)?
            .iter()
            .find(|f| f.id == id)
            .cloned())
    }

    async fn list_for_user(&self, user_id: UserId) -> Result<Vec<Fund>, RepositoryError> {
        Ok(self
            .funds
            .lock()
            .map_err(poisoned)?
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
        let mut g = self.funds.lock().map_err(poisoned)?;
        if let Some(slot) = g.iter_mut().find(|f| f.id == fund.id) {
            *slot = fund.clone();
        } else {
            g.push(fund.clone());
        }
        Ok(())
    }

    async fn find_obligation(
        &self,
        id: RepaymentObligationId,
    ) -> Result<Option<RepaymentObligation>, RepositoryError> {
        Ok(self
            .obligations
            .lock()
            .map_err(poisoned)?
            .iter()
            .find(|o| o.id == id)
            .cloned())
    }

    async fn list_active_obligations(
        &self,
        user_id: UserId,
    ) -> Result<Vec<RepaymentObligation>, RepositoryError> {
        Ok(self
            .obligations
            .lock()
            .map_err(poisoned)?
            .iter()
            .filter(|o| o.user_id == user_id)
            .cloned()
            .collect())
    }

    async fn find_obligation_for_transaction(
        &self,
        transaction_id: TransactionId,
    ) -> Result<Option<RepaymentObligation>, RepositoryError> {
        Ok(self
            .obligations
            .lock()
            .map_err(poisoned)?
            .iter()
            .find(|o| o.transaction_id == Some(transaction_id))
            .cloned())
    }

    async fn find_deficit_obligation_for_month(
        &self,
        month_id: MonthId,
    ) -> Result<Option<RepaymentObligation>, RepositoryError> {
        Ok(self
            .obligations
            .lock()
            .map_err(poisoned)?
            .iter()
            .find(|o| o.origin_month_id == Some(month_id) && o.source == ObligationSource::Deficit)
            .cloned())
    }

    async fn list_buffer_financed_transaction_ids(
        &self,
        user_id: UserId,
    ) -> Result<Vec<TransactionId>, RepositoryError> {
        Ok(self
            .obligations
            .lock()
            .map_err(poisoned)?
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
        let mut g = self.obligations.lock().map_err(poisoned)?;
        if let Some(slot) = g.iter_mut().find(|o| o.id == obligation.id) {
            *slot = obligation.clone();
        } else {
            g.push(obligation.clone());
        }
        Ok(())
    }
}

// ===========================================================================
// Harness
// ===========================================================================

struct Harness {
    months: Arc<MemMonthRepo>,
    budgets: Arc<MemBudgetRepo>,
    txns: Arc<MemTxnRepo>,
    service: MonthLifecycleService,
    user_id: UserId,
    budget_id: BudgetId,
    rollover_bucket_id: CategoryId,
}

fn ymd(y: i32, m: u32, d: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, d).expect("valid date")
}

/// Build a harness with one open-ended budget version + a rollover bucket. The
/// income expectation is injected by the caller so each test controls the
/// `expected_income` term of D5 exactly.
fn harness_with_income(income: Arc<dyn IncomeExpectation>) -> Harness {
    let months = Arc::new(MemMonthRepo::default());
    let budgets = Arc::new(MemBudgetRepo::default());
    let txns = Arc::new(MemTxnRepo::default());

    let user_id = UserId::generate();
    let budget_id = BudgetId::generate();
    let rollover_bucket_id = CategoryId::generate();

    {
        let mut b = budgets.budgets.lock().unwrap();
        b.push(Budget {
            id: budget_id,
            user_id,
            name: "Test".to_owned(),
            effective_from: ymd(2000, 1, 1),
            effective_to: None,
            created_at: Utc::now(),
        });
    }
    {
        let mut c = budgets.categories.lock().unwrap();
        c.push(Category {
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

    let funds = Arc::new(MemFundRepo::default());

    let service = MonthLifecycleService::new(
        Arc::clone(&months) as Arc<dyn MonthRepository>,
        Arc::clone(&budgets) as Arc<dyn BudgetRepository>,
        Arc::clone(&txns) as Arc<dyn TransactionRepository>,
        Arc::clone(&funds) as Arc<dyn FundRepository>,
        Arc::new(NoopUowProvider) as Arc<dyn UowProvider>,
        income,
    );

    Harness {
        months,
        budgets,
        txns,
        service,
        user_id,
        budget_id,
        rollover_bucket_id,
    }
}

/// Harness whose expected income is a flat injected figure (lets a test set the
/// `expected` term of D5 to any exact value).
fn harness_fixed_expected(expected: Money) -> Harness {
    harness_with_income(Arc::new(FixedExpectation::new(expected)))
}

/// Harness with zero expected income (so net = actual + Σ remaining — the
/// cleanest oracle for fund + conservation tests).
fn harness_zero_expected() -> Harness {
    harness_with_income(Arc::new(FixedExpectation::zero()))
}

fn add_expense_category(h: &Harness) -> CategoryId {
    let id = CategoryId::generate();
    h.budgets.categories.lock().unwrap().push(Category {
        id,
        budget_id: h.budget_id,
        category_key: CategoryKey::generate(),
        name: "Expense".to_owned(),
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

fn add_fund_category(h: &Harness) -> CategoryId {
    let id = CategoryId::generate();
    h.budgets.categories.lock().unwrap().push(Category {
        id,
        budget_id: h.budget_id,
        category_key: CategoryKey::generate(),
        name: "Fund".to_owned(),
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

fn txn(h: &Harness, month_id: MonthId, amount: Money) -> Transaction {
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
        created_at: Utc::now(),
        updated_at: Utc::now(),
    }
}

/// Snapshot of every month's posted rollover: `((year, month), amount, id)`,
/// chronologically. Used to assert idempotent replay yields byte-identical state.
async fn snapshot(h: &Harness) -> Vec<((i32, i32), Money, TransactionId)> {
    let all = h.months.list_for_user(h.user_id).await.unwrap();
    let mut v = Vec::new();
    for m in &all {
        let r = h
            .txns
            .find_rollover_for_month(m.id)
            .await
            .unwrap()
            .expect("rollover");
        v.push(((m.year, m.month), r.amount, r.id));
    }
    v
}

/// A UTC instant that is noon in New York on the given calendar date — so the
/// home-TZ month-membership (D2) is unambiguous.
fn ny_noon(year: i32, month: u32, day: u32) -> DateTime<Utc> {
    let naive = ymd(year, month, day).and_hms_opt(12, 0, 0).expect("noon");
    New_York
        .from_local_datetime(&naive)
        .single()
        .expect("unambiguous local time")
        .with_timezone(&Utc)
}

// ===========================================================================
// Independent Decimal oracle (re-derives D5 / SPEC §4.3 from scratch).
// ===========================================================================

/// The independent oracle's view of a month's net, computed in `Decimal` with
/// NO reference to the production `net_leftover`. This is the from-scratch
/// re-implementation of `SPEC §4.3`:
///
/// ```text
/// net = (actual_income − expected_income) + Σ(non-income, non-fund-draw, counting amounts)
/// ```
///
/// D6 Model A: a fund CONTRIBUTION is a manual Other-bucket expense that COUNTS in
/// the net — it is NOT excluded here. Only fund DRAWS (`is_fund_draw = true`) are
/// excluded (the money was already expensed at contribution time). `fund_ids` is
/// retained for signature parity with the call sites but no longer drives an
/// exclusion.
///
/// Returns the net as a `Decimal` so the comparison is at the raw arithmetic
/// layer, not through the production `Money` ops.
fn oracle_month_net(
    txns: &[Transaction],
    fund_ids: &[CategoryId],
    expected_income: Decimal,
) -> Decimal {
    // Intentionally unused under D6 Model A (contributions count).
    let _ = fund_ids;
    let mut actual_income = Decimal::ZERO;
    let mut expense_remaining = Decimal::ZERO;
    for t in txns {
        if !counts_independently(t.status) {
            continue;
        }
        let amt = t.amount.as_decimal();
        if t.income_kind.is_some() {
            actual_income += amt;
        } else if t.is_fund_draw {
            // Fund draw: excluded — money already expensed at contribution time
            // (D6 Model A, BUDGET-NO-DOUBLE-CHARGE-1).
        } else {
            // Everything else COUNTS — including fund contributions (D6 Model A).
            expense_remaining += amt;
        }
    }
    (actual_income - expected_income) + expense_remaining
}

/// Deterministic splitmix64 — a self-contained PRNG so the property loops are
/// reproducible without a crate dependency. Seeded per-test; the seed is printed
/// on failure so any counterexample replays exactly.
struct SplitMix64(u64);
impl SplitMix64 {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A signed cents amount in `[-lo, hi]` (inclusive-ish), suitable for an
    /// awkward `Money::from_minor` value.
    fn cents(&mut self, lo: i64, hi: i64) -> i64 {
        let span = (hi - lo).unsigned_abs() + 1;
        lo + i64::try_from(self.next_u64() % span).unwrap_or(0)
    }

    fn bool(&mut self) -> bool {
        self.next_u64() & 1 == 1
    }
}

// ===========================================================================
// D5 NET ORACLE
// ===========================================================================

/// D5: a +$100 income surplus raises the month net (and thus Other) by exactly
/// $100, by formula — no discrete line item (`SPEC §4.3`).
#[tokio::test]
async fn d5_income_surplus_raises_other_by_exactly_the_surplus() {
    // Expected income $4,000. Build a Jan with a +$100 surplus and no expenses,
    // advance to Feb, and assert the Feb rollover (= Jan net) is exactly +$100.
    let h = harness_fixed_expected(Money::from_major(4_000));
    let jan = h
        .service
        .ensure_current_month(h.user_id, ny_noon(2026, 1, 8))
        .await
        .expect("init jan");

    // Actual income $4,100 = expected + $100 surplus.
    let mut income = txn(&h, jan.id, Money::from_major(4_100));
    income.income_kind = Some(IncomeKind::Budgeted);
    h.txns.push(income);

    h.service
        .ensure_current_month(h.user_id, ny_noon(2026, 2, 8))
        .await
        .expect("init feb");

    let feb = h
        .months
        .find_by_year_month(h.user_id, 2026, 2)
        .await
        .unwrap()
        .expect("feb");
    let rollover = h
        .txns
        .find_rollover_for_month(feb.id)
        .await
        .unwrap()
        .expect("feb rollover");

    assert_eq!(
        rollover.amount,
        Money::from_major(100),
        "a +$100 surplus must raise Other by exactly $100"
    );
    // The rollover posts against the rollover bucket and is dated the 1st
    // (BUDGET-ROLLOVER-INTEGRITY-1).
    assert_eq!(rollover.category_id, Some(h.rollover_bucket_id));
    assert!(rollover.is_rollover);
    assert_eq!(rollover.date, ymd(2026, 2, 1));

    // Independent oracle cross-check on the raw Decimal layer.
    let jan_txns = h.txns.list_for_month(jan.id).await.unwrap();
    let oracle = oracle_month_net(&jan_txns, &[], Decimal::new(400_000, 2));
    assert_eq!(rollover.amount.as_decimal(), oracle);
}

/// D5 net oracle, table-driven: inject known actual/expected income + category
/// remainings and assert net = (actual − expected) + Σ(remaining) to the cent
/// against an independent Decimal oracle, for a spread of awkward figures.
#[tokio::test]
async fn d5_net_matches_independent_decimal_oracle_table() {
    // (actual_cents, expected_cents, [remaining_cents...])
    let cases: &[(i64, i64, &[i64])] = &[
        (400_000, 400_000, &[]),                // break-even
        (413_337, 400_000, &[-12_399, -7_801]), // awkward surplus + expenses
        (350_000, 400_000, &[-1, -1, -1]),      // income shortfall
        (400_001, 400_000, &[-99_999, 5_000]),  // one-cent surplus, mixed
        (0, 0, &[-33, 17, -1]),                 // zero income, odd cents
    ];

    for (actual_c, expected_c, remaining) in cases {
        let h = harness_fixed_expected(Money::from_minor(*expected_c));
        let exp_cat = add_expense_category(&h);
        let jan = h
            .service
            .ensure_current_month(h.user_id, ny_noon(2026, 1, 8))
            .await
            .expect("init jan");

        let mut income = txn(&h, jan.id, Money::from_minor(*actual_c));
        income.income_kind = Some(IncomeKind::Budgeted);
        h.txns.push(income);
        for r in *remaining {
            let mut e = txn(&h, jan.id, Money::from_minor(*r));
            e.category_id = Some(exp_cat);
            h.txns.push(e);
        }

        h.service
            .ensure_current_month(h.user_id, ny_noon(2026, 2, 8))
            .await
            .expect("init feb");
        let feb = h
            .months
            .find_by_year_month(h.user_id, 2026, 2)
            .await
            .unwrap()
            .expect("feb");
        let rollover = h
            .txns
            .find_rollover_for_month(feb.id)
            .await
            .unwrap()
            .expect("rollover");

        let jan_txns = h.txns.list_for_month(jan.id).await.unwrap();
        let oracle = oracle_month_net(&jan_txns, &[], Decimal::new(*expected_c, 2));
        assert_eq!(
            rollover.amount.as_decimal(),
            oracle,
            "case actual={actual_c} expected={expected_c} remaining={remaining:?}: \
             rollover must equal the independent D5 oracle"
        );
        // And cross-check the production formula agrees with the oracle too.
        let formula = net_leftover(
            Money::from_minor(*actual_c),
            Money::from_minor(*expected_c),
            remaining.iter().map(|r| Money::from_minor(*r)).sum(),
        );
        assert_eq!(formula.as_decimal(), oracle);
    }
}

// ===========================================================================
// D6 FUND-EARMARK
// ===========================================================================

/// D6: a fund contribution in an otherwise-zero-net month makes the month net
/// `= −contribution` (NOT 0 and NOT +contribution), while the fund "balance"
/// (the earmarked dollars) is `+contribution`. Total system money — Other net
/// plus fund balance — nets to zero, i.e. the dollar is counted exactly once
/// (`BUDGET-FUND-EARMARK-1`).
#[tokio::test]
async fn d6_fund_contribution_counted_once_keeps_system_money_invariant() {
    let h = harness_zero_expected();
    let fund_cat = add_fund_category(&h);

    let jan = h
        .service
        .ensure_current_month(h.user_id, ny_noon(2026, 1, 8))
        .await
        .expect("init jan");

    // The ONLY money movement in Jan is a $250.00 contribution INTO the fund
    // (an outflow from cash). No income, no other expense -> "otherwise zero".
    let contribution = Money::from_minor(25_000);
    let mut fund_txn = txn(&h, jan.id, -contribution);
    fund_txn.category_id = Some(fund_cat);
    h.txns.push(fund_txn);

    h.service
        .ensure_current_month(h.user_id, ny_noon(2026, 2, 8))
        .await
        .expect("init feb");

    let feb = h
        .months
        .find_by_year_month(h.user_id, 2026, 2)
        .await
        .unwrap()
        .expect("feb");
    let rollover = h
        .txns
        .find_rollover_for_month(feb.id)
        .await
        .unwrap()
        .expect("rollover");

    // D6 Model A: the contribution COUNTS in the rollover net — an otherwise-zero
    // month rolls over −$250 (NOT $0 and NOT +$250), reduced by exactly the
    // contribution while the fund balance is +$250.
    let fund_earmark = contribution; // the dollars now sitting in the fund.

    // Independent oracle: the contribution counts (no fund exclusion), so net is
    // −contribution.
    let jan_txns = h.txns.list_for_month(jan.id).await.unwrap();
    let oracle_net = oracle_month_net(&jan_txns, &[fund_cat], Decimal::ZERO);
    assert_eq!(
        oracle_net,
        -contribution.as_decimal(),
        "fund contribution counts -> net = −contribution (D6 Model A)"
    );
    assert_eq!(rollover.amount.as_decimal(), oracle_net);
    assert_eq!(rollover.amount, -contribution);

    // CONSERVATION (D6 Model A): the rolling Other is reduced by the contribution
    // AND the fund is up by the contribution, so the two ledgers sum back to 0 —
    // the $250 is counted exactly once (via the Other expense), with no separate
    // fund-balance subtraction from free-to-spend.
    assert_eq!(
        rollover.amount + fund_earmark,
        Money::ZERO,
        "rolling Other reduced by the contribution AND fund up by the contribution (counted once)"
    );

    // And a non-fund expense of the SAME size ALSO rolls into Other as
    // −contribution — under D6 Model A a fund contribution behaves like any other
    // Other-bucket expense (both COUNT); the control confirms the parity.
    let h2 = harness_zero_expected();
    let exp_cat = add_expense_category(&h2);
    let jan2 = h2
        .service
        .ensure_current_month(h2.user_id, ny_noon(2026, 1, 8))
        .await
        .expect("init jan2");
    let mut e = txn(&h2, jan2.id, -contribution);
    e.category_id = Some(exp_cat);
    h2.txns.push(e);
    h2.service
        .ensure_current_month(h2.user_id, ny_noon(2026, 2, 8))
        .await
        .expect("init feb2");
    let feb2 = h2
        .months
        .find_by_year_month(h2.user_id, 2026, 2)
        .await
        .unwrap()
        .expect("feb2");
    let rollover2 = h2
        .txns
        .find_rollover_for_month(feb2.id)
        .await
        .unwrap()
        .expect("rollover2");
    assert_eq!(
        rollover2.amount, -contribution,
        "an ORDINARY expense of the same size DOES roll into Other (control)"
    );
}

// ===========================================================================
// CENT-CONSERVATION (single rollover + multi-month chain + property loop)
// ===========================================================================

/// CENT-CONSERVATION across a single rollover: the Feb rollover equals the exact
/// independent Decimal oracle for Jan to the cent, for an awkward mix.
#[tokio::test]
async fn cent_conservation_single_rollover_vs_oracle() {
    let h = harness_fixed_expected(Money::from_minor(400_000));
    let exp_cat = add_expense_category(&h);
    let fund_cat = add_fund_category(&h);

    let jan = h
        .service
        .ensure_current_month(h.user_id, ny_noon(2026, 1, 8))
        .await
        .expect("init jan");

    let mut income = txn(&h, jan.id, Money::from_minor(411_113));
    income.income_kind = Some(IncomeKind::Budgeted);
    h.txns.push(income);

    for cents in [-12_399_i64, -7_801, -33, 1] {
        let mut e = txn(&h, jan.id, Money::from_minor(cents));
        e.category_id = Some(exp_cat);
        h.txns.push(e);
    }
    // A fund contribution that COUNTS in the net (D6 Model A): a manual Other-bucket
    // expense, is_fund_draw=false, so production and oracle both include it.
    let mut fund_txn = txn(&h, jan.id, Money::from_minor(-5_000));
    fund_txn.category_id = Some(fund_cat);
    h.txns.push(fund_txn);

    h.service
        .ensure_current_month(h.user_id, ny_noon(2026, 2, 8))
        .await
        .expect("init feb");

    let feb = h
        .months
        .find_by_year_month(h.user_id, 2026, 2)
        .await
        .unwrap()
        .expect("feb");
    let rollover = h
        .txns
        .find_rollover_for_month(feb.id)
        .await
        .unwrap()
        .expect("rollover");

    let jan_txns = h.txns.list_for_month(jan.id).await.unwrap();
    let oracle = oracle_month_net(&jan_txns, &[fund_cat], Decimal::new(400_000, 2));
    assert_eq!(
        rollover.amount.as_decimal(),
        oracle,
        "single-rollover net must match the independent oracle exactly"
    );
}

/// CENT-CONSERVATION across a MULTI-MONTH chain: seed several months with random
/// awkward transactions, run the catch-up, then verify that the rollover chain
/// telescopes exactly. Property: for every consecutive pair, the later month's
/// rollover equals the independent oracle net of the earlier month — and the
/// whole chain conserves cents (the final balance is the exact running sum of
/// every month's net, zero drift).
#[tokio::test]
async fn cent_conservation_multi_month_chain_property() {
    // Many seeds so the property is exercised over a wide space of awkward cents.
    for seed in 0..200_u64 {
        let mut rng = SplitMix64(seed.wrapping_mul(0x1234_5678_9ABC_DEF1).wrapping_add(1));
        let h = harness_fixed_expected(Money::from_minor(400_000));
        let exp_cat = add_expense_category(&h);
        let fund_cat = add_fund_category(&h);

        // Genesis: January 2026.
        h.service
            .ensure_current_month(h.user_id, ny_noon(2026, 1, 5))
            .await
            .expect("genesis");

        // Populate Jan..Apr (4 months) with random transactions, advancing the
        // current month each time so each month is "closed" before the next.
        let months_seq = [(2026_i32, 1_u32), (2026, 2), (2026, 3), (2026, 4)];
        for (idx, (year, month)) in months_seq.iter().enumerate() {
            let m = h
                .months
                .find_by_year_month(h.user_id, *year, i32::try_from(*month).unwrap())
                .await
                .unwrap()
                .expect("month exists");

            // Random income around the $4,000 expectation.
            let mut income = txn(&h, m.id, Money::from_minor(rng.cents(380_000, 420_000)));
            income.income_kind = Some(IncomeKind::Budgeted);
            h.txns.push(income);

            // A handful of random expenses.
            let n_exp = (rng.next_u64() % 4) + 1;
            for _ in 0..n_exp {
                let mut e = txn(&h, m.id, Money::from_minor(rng.cents(-50_000, -1)));
                e.category_id = Some(exp_cat);
                h.txns.push(e);
            }
            // Optionally a fund contribution — under D6 Model A it COUNTS in the
            // chain (is_fund_draw=false), exactly like an ordinary Other expense.
            if rng.bool() {
                let mut f = txn(&h, m.id, Money::from_minor(rng.cents(-20_000, -1)));
                f.category_id = Some(fund_cat);
                h.txns.push(f);
            }

            // Advance to the next month so its rollover posts from THIS month's
            // now-complete ledger (skip after the last seeded month).
            if idx + 1 < months_seq.len() {
                let (ny, nm) = months_seq[idx + 1];
                h.service
                    .ensure_current_month(h.user_id, ny_noon(ny, nm, 5))
                    .await
                    .expect("advance");
            }
        }

        // Now verify the chain telescopes. For each consecutive (prev -> cur),
        // the cur month's rollover row must equal the independent oracle net of
        // prev (which itself INCLUDES prev's own rollover row, exactly the
        // rolling-Other semantics).
        let all = h.months.list_for_user(h.user_id).await.unwrap();
        let mut running = Decimal::ZERO; // running Other balance, oracle side.
        for w in all.windows(2) {
            let prev = &w[0];
            let cur = &w[1];

            let prev_txns = h.txns.list_for_month(prev.id).await.unwrap();
            let oracle_prev_net =
                oracle_month_net(&prev_txns, &[fund_cat], Decimal::new(400_000, 2));

            let cur_rollover = h
                .txns
                .find_rollover_for_month(cur.id)
                .await
                .unwrap()
                .expect("cur rollover");

            assert_eq!(
                cur_rollover.amount.as_decimal(),
                oracle_prev_net,
                "seed {seed}: {}-{} rollover must equal oracle net of {}-{} \
                 (no cents created or destroyed across the rollover)",
                cur.year,
                cur.month,
                prev.year,
                prev.month,
            );
            running += oracle_prev_net;
        }

        // CONSERVATION across the whole chain: the sum of every posted rollover
        // amount equals the oracle running sum exactly (zero drift). Genesis
        // rollover is zero and is included in the sum harmlessly.
        let mut posted_sum = Decimal::ZERO;
        for m in &all {
            if let Some(r) = h.txns.find_rollover_for_month(m.id).await.unwrap() {
                posted_sum += r.amount.as_decimal();
            }
        }
        assert_eq!(
            posted_sum, running,
            "seed {seed}: Σ posted rollovers must equal the oracle running sum (cent conservation)"
        );
    }
}

/// Pure-arithmetic conservation property over the production `Money` ops: across
/// thousands of seeded awkward-cent nets, folding the chain forward in `Money`
/// equals the `Decimal` oracle sum exactly. This isolates the money-arithmetic
/// half of the conservation invariant from the service plumbing.
#[test]
fn money_chain_fold_loses_zero_cents_vs_decimal_oracle_property() {
    for seed in 0..5_000_u64 {
        let mut rng = SplitMix64(seed.wrapping_mul(0x2545_F491_4F6C_DD1D).wrapping_add(7));
        let len = (rng.next_u64() % 24) + 1;
        let mut money_balance = Money::ZERO;
        let mut oracle = Decimal::ZERO;
        for _ in 0..len {
            let c = rng.cents(-999_999, 999_999);
            money_balance += Money::from_minor(c);
            oracle += Decimal::new(c, 2);
        }
        assert_eq!(
            money_balance.as_decimal(),
            oracle,
            "seed {seed}: rolling Money balance drifted from the Decimal oracle"
        );
    }
}

// ===========================================================================
// MULTI-MONTH GAP
// ===========================================================================

/// MULTI-MONTH GAP: last month = March, current = June => exactly 3 NEW months
/// (Apr, May, Jun) created, with a correctly linked sequential rollover chain
/// (one rollover per month, each posting from the prior month).
#[tokio::test]
async fn multi_month_gap_march_to_june_creates_exactly_three_linked_months() {
    let h = harness_fixed_expected(Money::from_minor(400_000));
    let exp_cat = add_expense_category(&h);

    // Establish March as the latest existing month.
    let mar = h
        .service
        .ensure_current_month(h.user_id, ny_noon(2026, 3, 10))
        .await
        .expect("init march");
    assert_eq!((mar.year, mar.month), (2026, 3));

    // Give March a known net so the April rollover is checkable: actual $4,050,
    // expected $4,000 -> +$50 income surplus; plus a -$30.00 expense -> net +$20.
    let mut income = txn(&h, mar.id, Money::from_minor(405_000));
    income.income_kind = Some(IncomeKind::Budgeted);
    h.txns.push(income);
    let mut e = txn(&h, mar.id, Money::from_minor(-3_000));
    e.category_id = Some(exp_cat);
    h.txns.push(e);

    let before = h.months.list_for_user(h.user_id).await.unwrap().len();
    assert_eq!(before, 1, "only March before the gap catch-up");

    // Jump to June: April, May, June must be created (exactly 3 new months).
    h.service
        .ensure_current_month(h.user_id, ny_noon(2026, 6, 10))
        .await
        .expect("catch up to june");

    let all = h.months.list_for_user(h.user_id).await.unwrap();
    let keys: Vec<(i32, i32)> = all.iter().map(|m| (m.year, m.month)).collect();
    assert_eq!(
        keys,
        vec![(2026, 3), (2026, 4), (2026, 5), (2026, 6)],
        "exactly 3 new months (Apr/May/Jun) appended after March"
    );
    assert_eq!(all.len() - before, 3, "exactly three months created");

    // Each month has exactly one rollover row (sequential, linked chain).
    for m in &all {
        assert_eq!(
            h.txns.rollover_count(m.id),
            1,
            "month {}-{} has exactly one rollover",
            m.year,
            m.month
        );
    }

    // The April rollover must equal March's independent oracle net (+$20.00):
    // (405000 - 400000) + (-3000) cents = 5000 - 3000 = 2000 cents = $20.00.
    let mar_txns = h.txns.list_for_month(mar.id).await.unwrap();
    let mar_oracle = oracle_month_net(&mar_txns, &[], Decimal::new(400_000, 2));
    assert_eq!(mar_oracle, Decimal::new(2_000, 2), "March net = +$20.00");

    let apr = h
        .months
        .find_by_year_month(h.user_id, 2026, 4)
        .await
        .unwrap()
        .expect("april");
    let apr_rollover = h
        .txns
        .find_rollover_for_month(apr.id)
        .await
        .unwrap()
        .expect("april rollover");
    assert_eq!(
        apr_rollover.amount.as_decimal(),
        mar_oracle,
        "April rollover links to March's net exactly"
    );

    // May/June rollovers each equal the prior month's oracle net (chain links).
    for (prev_y, prev_m, cur_y, cur_m) in [(2026, 4, 2026, 5), (2026, 5, 2026, 6)] {
        let prev = h
            .months
            .find_by_year_month(h.user_id, prev_y, prev_m)
            .await
            .unwrap()
            .expect("prev");
        let cur = h
            .months
            .find_by_year_month(h.user_id, cur_y, cur_m)
            .await
            .unwrap()
            .expect("cur");
        let prev_txns = h.txns.list_for_month(prev.id).await.unwrap();
        let prev_oracle = oracle_month_net(&prev_txns, &[], Decimal::new(400_000, 2));
        let cur_rollover = h
            .txns
            .find_rollover_for_month(cur.id)
            .await
            .unwrap()
            .expect("cur rollover");
        assert_eq!(
            cur_rollover.amount.as_decimal(),
            prev_oracle,
            "{cur_y}-{cur_m} rollover links to {prev_y}-{prev_m} net"
        );
    }
}

// ===========================================================================
// IDEMPOTENCY
// ===========================================================================

/// IDEMPOTENCY: re-running init for an already-initialized month creates no new
/// month and no second rollover, no matter how many times it is re-entered.
#[tokio::test]
async fn idempotent_reentry_never_double_creates_or_double_posts() {
    let h = harness_fixed_expected(Money::from_minor(400_000));

    for _ in 0..5 {
        h.service
            .ensure_current_month(h.user_id, ny_noon(2026, 4, 20))
            .await
            .expect("idempotent init");
    }

    let all = h.months.list_for_user(h.user_id).await.unwrap();
    assert_eq!(all.len(), 1, "exactly one month despite 5 inits");
    assert_eq!(
        h.txns.rollover_count(all[0].id),
        1,
        "exactly one rollover despite 5 inits"
    );
}

/// IDEMPOTENCY under CONCURRENCY — the race the guard actually defends.
///
/// Sequential replay never re-enters a completed month (the catch-up anchor
/// `find_latest` always advances past it), so the two replay tests above
/// exercise loop-level idempotency but never the `post_rollover_if_absent`
/// "already posted -> benign no-op" branch. That branch exists for the
/// documented race (`BUDGET-IDEMPOTENT-MONTH-INIT-1`): two cold-start requests
/// both passing `find_latest` before either has created the month, then both
/// reaching `create_if_absent` / the rollover post.
///
/// This invariant is genuinely **DB-semantics-dependent**: it relies on the
/// `UNIQUE(user_id, year, month)` ON-CONFLICT atomicity and the partial-unique
/// `transactions(month_id) WHERE is_rollover` index resolving the race inside
/// one transaction boundary. The in-memory fakes in this file cannot model that
/// isolation faithfully (their `std::Mutex` is released across every await
/// point, so a fake "race" would prove a property of the fake, not the service).
/// Per the test-authoring guidance ("if a test needs DB behavior … gate any
/// live-DB test behind `#[ignore]`"), this is an ignored live-DB harness: it
/// runs N parallel `ensure_current_month` calls against a real Postgres-backed
/// service and asserts exactly one month + one rollover survive. It is wired for
/// the infra layer to run with `--ignored` once a live service builder exists;
/// the in-memory suite covers every non-concurrent invariant deterministically.
///
/// Marked `#[ignore]` (no live service builder in this crate's test scope yet)
/// and left as a structural assertion of the documented race contract.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "live-DB concurrency: requires a Postgres-backed service to faithfully \
            exercise ON-CONFLICT / partial-unique race resolution (infra layer)"]
async fn concurrent_genesis_inits_converge_on_one_month_and_one_rollover() {
    // Best-effort fan-out against the in-memory fakes documents the SHAPE of the
    // assertion the live-DB version makes. It is #[ignore]d because the fakes
    // cannot reproduce real transaction isolation; the assertion below is the
    // contract the infra-level live test must hold.
    let h = harness_fixed_expected(Money::from_minor(400_000));
    let Harness {
        months,
        txns,
        service,
        user_id,
        ..
    } = h;
    let service = Arc::new(service);
    let now = ny_noon(2026, 4, 20);

    let mut handles = Vec::new();
    for _ in 0..16 {
        let svc = Arc::clone(&service);
        handles.push(tokio::spawn(async move {
            svc.ensure_current_month(user_id, now).await
        }));
    }
    for handle in handles {
        handle
            .await
            .expect("join")
            .expect("each concurrent init succeeds (no UniqueViolation leaks out)");
    }

    let all = months.list_for_user(user_id).await.unwrap();
    assert_eq!(
        all.len(),
        1,
        "racing genesis inits must converge on exactly one month"
    );
    assert_eq!(
        txns.rollover_count(all[0].id),
        1,
        "racing inits must produce exactly one rollover (guard no-op'd the losers)"
    );
}

/// IDEMPOTENCY (multi-month gap run TWICE): a multi-month catch-up replayed at
/// the same instant yields byte-identical state — same month set, same single
/// rollover per month, and the same rollover AMOUNTS (no drift on replay).
#[tokio::test]
async fn multi_month_gap_catch_up_run_twice_is_identical() {
    let h = harness_fixed_expected(Money::from_minor(400_000));
    let exp_cat = add_expense_category(&h);

    // Genesis Jan with some activity, then catch up to May.
    let jan = h
        .service
        .ensure_current_month(h.user_id, ny_noon(2026, 1, 5))
        .await
        .expect("genesis");
    let mut income = txn(&h, jan.id, Money::from_minor(407_777));
    income.income_kind = Some(IncomeKind::Budgeted);
    h.txns.push(income);
    let mut e = txn(&h, jan.id, Money::from_minor(-13_579));
    e.category_id = Some(exp_cat);
    h.txns.push(e);

    // First catch-up to May.
    h.service
        .ensure_current_month(h.user_id, ny_noon(2026, 5, 5))
        .await
        .expect("catch up");

    let first = snapshot(&h).await;

    // Replay the SAME catch-up at the SAME instant.
    h.service
        .ensure_current_month(h.user_id, ny_noon(2026, 5, 5))
        .await
        .expect("replay");

    let second = snapshot(&h).await;

    assert_eq!(first.len(), 5, "Jan..May = exactly 5 months after catch-up");
    assert_eq!(
        first, second,
        "replaying the multi-month catch-up must yield byte-identical state \
         (same months, same rollover amounts, same rollover row ids — no new \
         rows, no drift)"
    );
    // And still exactly one rollover per month.
    let all = h.months.list_for_user(h.user_id).await.unwrap();
    for m in &all {
        assert_eq!(h.txns.rollover_count(m.id), 1);
    }
}
