//! Independent property + oracle tests for the **fund service** (build step 5;
//! `SPEC §4.7` sinking funds, `§4.9` buffer/surplus/large-purchase, D6/D7).
//!
//! These tests were authored by a SEPARATE test-author agent that did **not**
//! trust the build's own `src/fund/tests.rs` (`ORCH-REVIEWER-SPLIT-1` spirit).
//! Phase-1 audit flagged the rolling-Other net and the fund earmark as the
//! highest double-count risk in the codebase, so this file:
//!   * builds its **own** in-memory fakes from scratch against the crate's
//!     *public* trait surface only (it never imports the build's test fakes), and
//!   * carries its **own** `rust_decimal` oracle that re-derives the D5 net and
//!     the whole-system-money conservation invariants independently of any
//!     production predicate. Where it cross-checks the production
//!     `net_leftover`, that is an *additional* assertion, never the sole oracle.
//!
//! The fakes deliberately wire the fund service AND the month-lifecycle service
//! over the SAME backing stores, so an earmark written by `FundService` is read
//! back by the real `MonthLifecycleService` rollover — exercising the D6 single-
//! counting seam end to end, not just the fund-side bookkeeping.
//!
//! ## Invariants covered (`ORCH-NEW-PATH-TESTS-1`, `PROC-REGRESSION-TEST-1`)
//!
//!   * EARMARK SINGLE-COUNTING (`BUDGET-FUND-EARMARK-1`, D6): a `contribute()`
//!     into a fund in an otherwise-zero-net month makes the rolling-Other net =
//!     0 (the build's *total* exclusion; the brief's "−contribution" framing is
//!     a different model — see the test's note), the fund balance = +amount, and
//!     the total-system-money invariant holds: Other-net + fund-balance equals
//!     the cash moved, counted exactly once. A control (ordinary expense of the
//!     same size) DOES roll into Other, proving the exclusion is the fund's doing.
//!   * BUFFER-FINANCED PURCHASE (`BUDGET-NO-DOUBLE-CHARGE-1`, D7): the full-price
//!     row posts for tracking but the financed month's budget impact via the
//!     rollover is ZERO; the buffer is drawn by the full price; an obligation is
//!     created; the installments restore the buffer toward (not beyond)
//!     `target_balance` until remaining = 0 -> status = paid; cent-conservation
//!     across the whole arc (purchase + N installments) — no money created or
//!     destroyed (property test over many awkward price/month splits).
//!   * SURPLUS DRAW (D7): a surplus draw is a fund-draw with NO repayment and is
//!     NOT re-charged as a budget expense (it is excluded from the rolling net,
//!     so it is not double-counted against the contribution that funded it).
//!   * SINKING FUND (`SPEC §4.7`): monthly accrual increases `fund_balance` by
//!     `amount / period_months` and CARRIES OVER across accruals (does not
//!     reset); reset-on-payment draws the reserve to/below zero of the obligation
//!     and re-anchors the accrual clock forward (forward-looking from the payout
//!     date).
//!   * BUFFER HEALTH: advisory only — it never blocks a draw (a draw below a
//!     "caution" target still succeeds and mutates state).
//!
//! Property tests are written as deterministic generative loops over a seeded
//! splitmix64 PRNG rather than pulling in `proptest`: adding a workspace
//! dependency is a structural change that routes to a human (`AGENTS.md`); a
//! seeded loop over thousands of awkward-cent cases gives the same coverage with
//! a reproducible failure (the seed prints on failure so a counterexample
//! replays exactly).
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
#![allow(clippy::too_many_arguments)]

use std::any::Any;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use chrono_tz::America::New_York;
use rust_decimal::Decimal;

use budget_app_services::income::{FixedExpectation, IncomeExpectation};
use budget_app_services::{
    BufferHealth, FundService, LargePurchaseResolution, MonthLifecycleService,
};

use budget_domain::budget::Budget;
use budget_domain::category::Category;
use budget_domain::enums::{
    Cadence, CategoryGrp, FundKind, IncomeKind, ObligationStatus, TransactionSource,
    TransactionStatus,
};
use budget_domain::fund::Fund;
use budget_domain::ids::{
    BudgetId, CategoryId, CategoryKey, FundId, MonthId, RepaymentObligationId, TransactionId,
    UserId,
};
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
// Deliberately NOT the build's `src/fund/tests.rs` fakes; re-derived here so the
// test does not inherit any bug the build's fakes might share with the code.
// ===========================================================================

/// No-op unit-of-work handle. The fakes have no real transaction; the handle
/// exists only to satisfy the `as_any` downcast surface.
struct NoopUow;
impl UnitOfWork for NoopUow {
    fn as_any(&self) -> &dyn Any {
        self
    }
}

type BoxedClosure<'a> =
    Box<dyn for<'u> FnOnce(&'u dyn UnitOfWork) -> UowFuture<'u, Box<dyn Any + Send>> + Send + 'a>;

/// Runs the closure with a no-op handle. Atomicity is exercised structurally;
/// real commit/rollback is an infra concern.
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
    fn push(&self, t: Transaction) {
        self.txns.lock().unwrap().push(t);
    }

    fn count(&self) -> usize {
        self.txns.lock().unwrap().len()
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

    async fn category_spent_for_month(
        &self,
        _month_id: MonthId,
    ) -> Result<Vec<CategorySpent>, RepositoryError> {
        Ok(Vec::new())
    }

    async fn month_net(&self, month_id: MonthId) -> Result<MonthNet, RepositoryError> {
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
        // Partial-unique transactions(month_id) WHERE is_rollover.
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
/// re-derived locally so the oracle shares no code with production.
fn counts_independently(status: TransactionStatus) -> bool {
    matches!(
        status,
        TransactionStatus::Settled | TransactionStatus::Expected
    )
}

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
        // ACTIVE only — independent of the build: filter on status, not "all".
        Ok(self
            .obligations
            .lock()
            .map_err(poisoned)?
            .iter()
            .filter(|o| o.user_id == user_id && o.status == ObligationStatus::Active)
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
            .find(|o| o.transaction_id == transaction_id)
            .cloned())
    }

    async fn list_buffer_financed_transaction_ids(
        &self,
        user_id: UserId,
    ) -> Result<Vec<TransactionId>, RepositoryError> {
        // EVERY obligation's tracking txn, active OR paid (D7: the full price stays
        // excluded forever because the cash was buffer-fronted).
        Ok(self
            .obligations
            .lock()
            .map_err(poisoned)?
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
// Harness: fund service + month-lifecycle service over the SAME stores.
// ===========================================================================

struct Harness {
    months: Arc<MemMonthRepo>,
    budgets: Arc<MemBudgetRepo>,
    txns: Arc<MemTxnRepo>,
    funds: Arc<MemFundRepo>,
    fund_service: FundService,
    lifecycle: MonthLifecycleService,
    user_id: UserId,
    budget_id: BudgetId,
}

fn ymd(y: i32, m: u32, d: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, d).expect("valid date")
}

fn now_ts() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 1, 15, 12, 0, 0)
        .single()
        .expect("ts")
}

/// A UTC instant that is noon in New York on the given calendar date, so the
/// home-TZ month-membership (D2) is unambiguous.
fn ny_noon(year: i32, month: u32, day: u32) -> DateTime<Utc> {
    let naive = ymd(year, month, day).and_hms_opt(12, 0, 0).expect("noon");
    New_York
        .from_local_datetime(&naive)
        .single()
        .expect("unambiguous local time")
        .with_timezone(&Utc)
}

/// Build a harness with one open-ended budget version + a rollover bucket, both
/// services wired over the same backing repos. `expected_income` is injected so
/// each test controls the D5 `expected` term exactly.
fn harness_with_expected(expected: Money) -> Harness {
    let months = Arc::new(MemMonthRepo::default());
    let budgets = Arc::new(MemBudgetRepo::default());
    let txns = Arc::new(MemTxnRepo::default());
    let funds = Arc::new(MemFundRepo::default());

    let user_id = UserId::generate();
    let budget_id = BudgetId::generate();
    let rollover_bucket_id = CategoryId::generate();

    budgets.budgets.lock().unwrap().push(Budget {
        id: budget_id,
        user_id,
        name: "Test".to_owned(),
        effective_from: ymd(2000, 1, 1),
        effective_to: None,
        created_at: Utc::now(),
    });
    budgets.categories.lock().unwrap().push(Category {
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

    let income: Arc<dyn IncomeExpectation> = Arc::new(FixedExpectation::new(expected));

    let fund_service = FundService::new(
        Arc::clone(&funds) as Arc<dyn FundRepository>,
        Arc::clone(&txns) as Arc<dyn TransactionRepository>,
        Arc::clone(&budgets) as Arc<dyn BudgetRepository>,
        Arc::new(NoopUowProvider) as Arc<dyn UowProvider>,
    );
    let lifecycle = MonthLifecycleService::new(
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
        funds,
        fund_service,
        lifecycle,
        user_id,
        budget_id,
    }
}

fn harness_zero_expected() -> Harness {
    harness_with_expected(Money::ZERO)
}

impl Harness {
    /// Add a plain (non-fund) expense category and return its id.
    fn add_expense_category(&self) -> CategoryId {
        let id = CategoryId::generate();
        self.budgets.categories.lock().unwrap().push(Category {
            id,
            budget_id: self.budget_id,
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

    /// Add a fund-bound earmark category (annual cadence => a sinking fund, so
    /// the month-lifecycle netting excludes contributions assigned to it). The
    /// `contribute()` / installment earmark target is exactly such a category.
    fn add_fund_category(&self, period_months: i32, amount: Money) -> CategoryId {
        let id = CategoryId::generate();
        self.budgets.categories.lock().unwrap().push(Category {
            id,
            budget_id: self.budget_id,
            category_key: CategoryKey::generate(),
            name: "Fund".to_owned(),
            amount,
            grp: CategoryGrp::Fixed,
            settle_type: None,
            expected_bills: None,
            is_rollover_bucket: false,
            cadence: Cadence::Annual,
            period_months: Some(period_months),
            fund_balance: Money::ZERO,
            next_due_date: None,
            sort_order: 2,
        });
        id
    }

    fn push_buffer(&self, balance: Money, target: Money) -> FundId {
        let id = FundId::generate();
        self.funds.funds.lock().unwrap().push(Fund {
            id,
            user_id: self.user_id,
            name: "Buffer".to_owned(),
            kind: FundKind::Buffer,
            balance,
            target_balance: Some(target),
            compulsory_repayment: true,
            created_at: now_ts(),
        });
        id
    }

    fn push_surplus(&self, balance: Money) -> FundId {
        let id = FundId::generate();
        self.funds.funds.lock().unwrap().push(Fund {
            id,
            user_id: self.user_id,
            name: "Surplus".to_owned(),
            kind: FundKind::Surplus,
            balance,
            target_balance: None,
            compulsory_repayment: false,
            created_at: now_ts(),
        });
        id
    }

    async fn fund_balance(&self, fund_id: FundId) -> Money {
        self.funds
            .find_by_id(fund_id)
            .await
            .unwrap()
            .expect("fund")
            .balance
    }

    async fn category_fund_balance(&self, category_id: CategoryId) -> Money {
        self.budgets
            .find_category(category_id)
            .await
            .unwrap()
            .expect("category")
            .fund_balance
    }

    async fn rollover_of(&self, year: i32, month: i32) -> Money {
        let m = self
            .months
            .find_by_year_month(self.user_id, year, month)
            .await
            .unwrap()
            .expect("month");
        self.txns
            .find_rollover_for_month(m.id)
            .await
            .unwrap()
            .expect("rollover")
            .amount
    }
}

/// A plain settled expense transaction in `month_id` for `amount`.
fn expense_txn(h: &Harness, month_id: MonthId, category: CategoryId, amount: Money) -> Transaction {
    Transaction {
        id: TransactionId::generate(),
        user_id: h.user_id,
        month_id,
        category_id: Some(category),
        account_id: None,
        date: ymd(2026, 1, 15),
        amount,
        description: "e".to_owned(),
        source: TransactionSource::Manual,
        plaid_transaction_id: None,
        status: TransactionStatus::Settled,
        income_kind: None,
        is_rollover: false,
        created_at: now_ts(),
        updated_at: now_ts(),
    }
}

// ===========================================================================
// Independent Decimal oracle.
// ===========================================================================

/// The independent oracle's view of a month's net, in `Decimal`, with NO
/// reference to the production `net_leftover` or `counts_in_month_expense_remaining`:
///
/// ```text
/// net = (actual_income − expected_income) + Σ(non-income, non-fund, non-buffer-financed amounts)
/// ```
///
/// `fund_category_ids` = the categories that earmark money into a fund (sinking
/// accrual / contribution / installment targets). `buffer_financed_ids` = the
/// full-price tracking txns. Both are excluded so the dollar is counted once.
fn oracle_month_net(
    txns: &[Transaction],
    fund_category_ids: &[CategoryId],
    buffer_financed_ids: &[TransactionId],
    expected_income: Decimal,
) -> Decimal {
    let mut actual_income = Decimal::ZERO;
    let mut expense_remaining = Decimal::ZERO;
    for t in txns {
        if !counts_independently(t.status) {
            continue;
        }
        let amt = t.amount.as_decimal();
        if t.income_kind.is_some() {
            actual_income += amt;
        } else if t
            .category_id
            .is_some_and(|c| fund_category_ids.contains(&c))
        {
            // Fund contribution: excluded from the net (D6).
        } else if buffer_financed_ids.contains(&t.id) {
            // Buffer-financed full price: excluded from the net (D7).
        } else {
            expense_remaining += amt;
        }
    }
    (actual_income - expected_income) + expense_remaining
}

/// Deterministic splitmix64 — reproducible property loops without a crate dep.
struct SplitMix64(u64);
impl SplitMix64 {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A positive cents amount in `[lo, hi]` inclusive.
    fn cents(&mut self, lo: i64, hi: i64) -> i64 {
        let span = (hi - lo).unsigned_abs() + 1;
        lo + i64::try_from(self.next_u64() % span).unwrap_or(0)
    }
}

// ===========================================================================
// EARMARK SINGLE-COUNTING (D6, BUDGET-FUND-EARMARK-1)
// ===========================================================================

/// D6: a `contribute()` into a fund in an otherwise-zero-net month leaves the
/// rolling-Other net at 0 (the contribution is EXCLUDED from the net) while the
/// fund balance is +amount. Total system money — Other net + fund balance —
/// equals the cash moved out of free-to-spend, counted exactly once.
///
/// NOTE on the brief's "net = −contribution" framing: that describes a model in
/// which the contribution counts as a month expense AND is then excluded from
/// the carryover chain. In THIS build the exclusion is TOTAL — the contribution
/// never enters the net, so the carried-over net is 0. The conservation property
/// (Other-net + fund-earmark == the contribution, no double-count, no loss) is
/// the real invariant and is asserted directly; the divergence from the brief's
/// wording is flagged here, not silently absorbed.
#[tokio::test]
async fn earmark_contribution_excluded_from_net_and_counted_once() {
    let h = harness_zero_expected();
    // 12-month sinking-style fund category so the lifecycle netting excludes it.
    let fund_cat = h.add_fund_category(12, Money::from_major(1_200));
    let fund_id = h.push_surplus(Money::ZERO);

    let jan = h
        .lifecycle
        .ensure_current_month(h.user_id, ny_noon(2026, 1, 8))
        .await
        .expect("init jan");

    let contribution = Money::from_minor(25_000); // $250.00
    let returned = h
        .fund_service
        .contribute(
            fund_id,
            jan.id,
            fund_cat,
            contribution,
            ymd(2026, 1, 10),
            now_ts(),
        )
        .await
        .expect("contribute");

    // Fund balance bumped by exactly the contribution.
    assert_eq!(returned.balance, contribution);
    assert_eq!(h.fund_balance(fund_id).await, contribution);

    // Advance to Feb so the Jan rollover posts from Jan's now-complete ledger.
    h.lifecycle
        .ensure_current_month(h.user_id, ny_noon(2026, 2, 8))
        .await
        .expect("init feb");

    let feb_rollover = h.rollover_of(2026, 2).await;

    // The contribution is EXCLUDED from the rollover net: an otherwise-zero month
    // rolls over $0, NOT −$250 and NOT +$250.
    assert_eq!(
        feb_rollover,
        Money::ZERO,
        "fund contribution must be excluded from the rolling-Other net (D6)"
    );

    // Independent oracle agrees the net is 0 with the fund category excluded.
    let jan_txns = h.txns.list_for_month(jan.id).await.unwrap();
    let oracle = oracle_month_net(&jan_txns, &[fund_cat], &[], Decimal::ZERO);
    assert_eq!(oracle, Decimal::ZERO);
    assert_eq!(feb_rollover.as_decimal(), oracle);

    // CONSERVATION: Other net + fund earmark == the cash moved out of free-to-
    // spend, i.e. the $250 is represented exactly once across the two ledgers.
    assert_eq!(
        feb_rollover + h.fund_balance(fund_id).await,
        contribution,
        "the earmarked dollar must be counted exactly once across Other + fund"
    );
}

/// Control: an ORDINARY expense of the same size DOES roll into Other — proving
/// the exclusion above is specifically the fund category's doing, not a quirk of
/// the zero-net setup.
#[tokio::test]
async fn earmark_control_ordinary_expense_does_roll_into_other() {
    let h = harness_zero_expected();
    let exp_cat = h.add_expense_category();
    let jan = h
        .lifecycle
        .ensure_current_month(h.user_id, ny_noon(2026, 1, 8))
        .await
        .expect("init jan");

    let amount = Money::from_minor(25_000);
    h.txns.push(expense_txn(&h, jan.id, exp_cat, -amount));

    h.lifecycle
        .ensure_current_month(h.user_id, ny_noon(2026, 2, 8))
        .await
        .expect("init feb");

    assert_eq!(
        h.rollover_of(2026, 2).await,
        -amount,
        "an ordinary (non-fund) expense rolls into Other in full (control)"
    );
}

/// EARMARK property: across many awkward contribution amounts, the fund balance
/// equals exactly the sum of contributions and the rolling net stays 0 (each
/// earmarked dollar counted once). Conservation across the whole month.
#[tokio::test]
async fn earmark_property_many_contributions_conserve_and_net_zero() {
    for seed in 0..150_u64 {
        let mut rng = SplitMix64(seed.wrapping_mul(0x1234_5678_9ABC_DEF1).wrapping_add(1));
        let h = harness_zero_expected();
        let fund_cat = h.add_fund_category(12, Money::from_major(1_200));
        let fund_id = h.push_surplus(Money::ZERO);

        let jan = h
            .lifecycle
            .ensure_current_month(h.user_id, ny_noon(2026, 1, 8))
            .await
            .expect("init jan");

        let n = (rng.next_u64() % 5) + 1;
        let mut total = Money::ZERO;
        let mut oracle_total = Decimal::ZERO;
        for _ in 0..n {
            let c = rng.cents(1, 50_000);
            let amount = Money::from_minor(c);
            h.fund_service
                .contribute(
                    fund_id,
                    jan.id,
                    fund_cat,
                    amount,
                    ymd(2026, 1, 10),
                    now_ts(),
                )
                .await
                .expect("contribute");
            total += amount;
            oracle_total += Decimal::new(c, 2);
        }

        // Fund balance == Σ contributions exactly (no drift).
        assert_eq!(
            h.fund_balance(fund_id).await.as_decimal(),
            oracle_total,
            "seed {seed}: fund balance must equal the exact sum of contributions"
        );

        h.lifecycle
            .ensure_current_month(h.user_id, ny_noon(2026, 2, 8))
            .await
            .expect("init feb");

        let rollover = h.rollover_of(2026, 2).await;
        assert_eq!(
            rollover,
            Money::ZERO,
            "seed {seed}: all contributions excluded -> net 0"
        );
        // Conservation across both ledgers.
        assert_eq!(
            rollover + h.fund_balance(fund_id).await,
            total,
            "seed {seed}: earmarked dollars counted exactly once across Other + fund"
        );
    }
}

// ===========================================================================
// BUFFER-FINANCED PURCHASE (D7, BUDGET-NO-DOUBLE-CHARGE-1)
// ===========================================================================

/// D7: a buffer-financed purchase posts the full-price row (for tracking) but the
/// financed month's budget impact via the rollover is ZERO; the buffer is drawn
/// by the full price; an obligation is created. Then the installments restore the
/// buffer toward target until remaining = 0 -> paid, and the whole arc conserves
/// cents.
#[tokio::test]
async fn buffer_financed_full_price_zero_budget_impact_buffer_drawn_obligation_created() {
    let h = harness_zero_expected();
    let earmark = h.add_fund_category(12, Money::from_major(1_200)); // installment target
    let target = Money::from_major(5_000);
    let start_balance = Money::from_major(5_000);
    let fund_id = h.push_buffer(start_balance, target);

    let jan = h
        .lifecycle
        .ensure_current_month(h.user_id, ny_noon(2026, 1, 8))
        .await
        .expect("init jan");

    let price = Money::from_major(1_200);
    let months = 3;
    let txn_id = h
        .fund_service
        .record_large_purchase(
            h.user_id,
            jan.id,
            earmark,
            price,
            "TV".to_owned(),
            ymd(2026, 1, 12),
            LargePurchaseResolution::BufferFinanced { fund_id, months },
            now_ts(),
        )
        .await
        .expect("buffer finance");

    // Buffer drawn by the full price.
    assert_eq!(
        h.fund_balance(fund_id).await,
        start_balance - price,
        "buffer drawn by the full price at purchase"
    );

    // Obligation created, keyed to the tracking txn.
    let obligation = h
        .funds
        .find_obligation_for_transaction(txn_id)
        .await
        .unwrap()
        .expect("obligation");
    assert_eq!(obligation.total_amount, price);
    assert_eq!(obligation.remaining_amount, price);
    assert_eq!(obligation.status, ObligationStatus::Active);

    // The full-price row exists (for tracking).
    let tracking = h.txns.find_by_id(txn_id).await.unwrap().expect("tracking");
    assert_eq!(tracking.amount, -price);

    // Financed month's budget impact via the rollover is ZERO (the full price is
    // excluded; nothing else happened in Jan).
    h.lifecycle
        .ensure_current_month(h.user_id, ny_noon(2026, 2, 8))
        .await
        .expect("init feb");
    let feb_rollover = h.rollover_of(2026, 2).await;
    assert_eq!(
        feb_rollover,
        Money::ZERO,
        "buffer-financed full price has ZERO month-budget impact (D7)"
    );

    // Oracle cross-check: with the tracking txn excluded, Jan net is 0.
    let jan_txns = h.txns.list_for_month(jan.id).await.unwrap();
    let oracle = oracle_month_net(&jan_txns, &[earmark], &[txn_id], Decimal::ZERO);
    assert_eq!(oracle, Decimal::ZERO);
    assert_eq!(feb_rollover.as_decimal(), oracle);
}

/// D7 installment arc: post all installments; the obligation runs to remaining=0
/// -> status=paid, the buffer is restored to EXACTLY its starting balance (back
/// to target, not beyond), and Σ installments == the full price to the cent.
#[tokio::test]
async fn buffer_financed_installments_restore_to_target_and_conserve_cents() {
    let h = harness_zero_expected();
    let earmark = h.add_fund_category(12, Money::from_major(1_200));
    let start_balance = Money::from_major(5_000);
    let fund_id = h.push_buffer(start_balance, start_balance);

    let jan = h
        .lifecycle
        .ensure_current_month(h.user_id, ny_noon(2026, 1, 8))
        .await
        .expect("init jan");

    // Awkward split: $100.00 / 3 = $33.33, $33.33, $33.34.
    let price = Money::from_major(100);
    let months = 3;
    let txn_id = h
        .fund_service
        .record_large_purchase(
            h.user_id,
            jan.id,
            earmark,
            price,
            "Thing".to_owned(),
            ymd(2026, 1, 12),
            LargePurchaseResolution::BufferFinanced { fund_id, months },
            now_ts(),
        )
        .await
        .expect("buffer finance");

    let obligation_id = h
        .funds
        .find_obligation_for_transaction(txn_id)
        .await
        .unwrap()
        .expect("obligation")
        .id;

    // Buffer drawn to 5000 - 100 = 4900.
    assert_eq!(h.fund_balance(fund_id).await, start_balance - price);

    // Post the three installments.
    let mut last = None;
    for _ in 0..months {
        last = Some(
            h.fund_service
                .post_installment(obligation_id, jan.id, earmark, ymd(2026, 1, 20), now_ts())
                .await
                .expect("installment"),
        );
    }
    let final_ob = last.expect("an installment");

    // Paid, remaining 0, months_remaining 0.
    assert_eq!(final_ob.status, ObligationStatus::Paid);
    assert!(final_ob.remaining_amount.is_zero());
    assert_eq!(final_ob.months_remaining, 0);

    // Buffer restored to EXACTLY the starting balance (= target): not beyond.
    assert_eq!(
        h.fund_balance(fund_id).await,
        start_balance,
        "installments restore the buffer to target exactly (no overshoot)"
    );

    // Σ installments == full price (cent conservation across the arc).
    let installment_sum: Money = h
        .txns
        .txns
        .lock()
        .unwrap()
        .iter()
        .filter(|t| t.month_id == jan.id && t.description == "Buffer repayment installment")
        .map(|t| -t.amount) // installments are negative expenses; magnitude
        .sum();
    assert_eq!(
        installment_sum, price,
        "Σ installments must equal the full price exactly ($33.33+$33.33+$33.34)"
    );
}

/// D7 cent-conservation PROPERTY across many awkward (price, months) splits:
/// for each, post every installment and assert
///   (a) Σ installments == price exactly,
///   (b) the buffer ends at its starting balance (full draw fully restored), and
///   (c) the obligation ends paid with remaining 0.
/// Money is neither created nor destroyed across purchase + N installments.
#[tokio::test]
async fn buffer_financed_arc_conserves_cents_property() {
    for seed in 0..400_u64 {
        let mut rng = SplitMix64(seed.wrapping_mul(0x2545_F491_4F6C_DD1D).wrapping_add(3));
        let h = harness_zero_expected();
        let earmark = h.add_fund_category(12, Money::from_major(1_200));
        let start_balance = Money::from_major(10_000);
        let fund_id = h.push_buffer(start_balance, start_balance);

        let jan = h
            .lifecycle
            .ensure_current_month(h.user_id, ny_noon(2026, 1, 8))
            .await
            .expect("init jan");

        let price_cents = rng.cents(1, 900_000); // up to $9,000
        let price = Money::from_minor(price_cents);
        let months = i32::try_from((rng.next_u64() % 18) + 1).unwrap_or(1);

        let txn_id = h
            .fund_service
            .record_large_purchase(
                h.user_id,
                jan.id,
                earmark,
                price,
                "P".to_owned(),
                ymd(2026, 1, 12),
                LargePurchaseResolution::BufferFinanced { fund_id, months },
                now_ts(),
            )
            .await
            .expect("buffer finance");

        let obligation_id = h
            .funds
            .find_obligation_for_transaction(txn_id)
            .await
            .unwrap()
            .expect("obligation")
            .id;

        // Post installments until paid (cap iterations defensively at months + 2;
        // if the obligation is not paid by then, that itself is a failure).
        let mut posted = 0;
        loop {
            let ob = h
                .funds
                .find_obligation(obligation_id)
                .await
                .unwrap()
                .unwrap();
            if ob.status == ObligationStatus::Paid {
                break;
            }
            h.fund_service
                .post_installment(obligation_id, jan.id, earmark, ymd(2026, 1, 20), now_ts())
                .await
                .expect("installment");
            posted += 1;
            assert!(
                posted <= months + 2,
                "seed {seed}: price={price_cents} months={months}: obligation did not \
                 reach paid within the scheduled installments (runaway repayment)"
            );
        }

        // (a) Σ installments == price exactly.
        let installment_sum: Money = h
            .txns
            .txns
            .lock()
            .unwrap()
            .iter()
            .filter(|t| t.month_id == jan.id && t.description == "Buffer repayment installment")
            .map(|t| -t.amount)
            .sum();
        assert_eq!(
            installment_sum, price,
            "seed {seed}: price={price_cents} months={months}: Σ installments must equal price"
        );

        // (b) buffer fully restored to start (draw == Σ installments).
        assert_eq!(
            h.fund_balance(fund_id).await,
            start_balance,
            "seed {seed}: price={price_cents} months={months}: buffer must return to start exactly"
        );

        // (c) obligation paid, remaining 0.
        let final_ob = h
            .funds
            .find_obligation(obligation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(final_ob.status, ObligationStatus::Paid);
        assert!(final_ob.remaining_amount.is_zero());
        assert!(
            posted <= months,
            "seed {seed}: price={price_cents} months={months}: must not exceed scheduled installments"
        );
    }
}

/// D7: posting an installment against an ALREADY-PAID obligation is rejected
/// (no overshoot, no negative remaining, no money created).
#[tokio::test]
async fn buffer_financed_over_payment_is_rejected() {
    let h = harness_zero_expected();
    let earmark = h.add_fund_category(12, Money::from_major(1_200));
    let fund_id = h.push_buffer(Money::from_major(5_000), Money::from_major(5_000));

    let jan = h
        .lifecycle
        .ensure_current_month(h.user_id, ny_noon(2026, 1, 8))
        .await
        .expect("init jan");

    let txn_id = h
        .fund_service
        .record_large_purchase(
            h.user_id,
            jan.id,
            earmark,
            Money::from_major(60),
            "X".to_owned(),
            ymd(2026, 1, 12),
            LargePurchaseResolution::BufferFinanced { fund_id, months: 2 },
            now_ts(),
        )
        .await
        .expect("buffer finance");
    let obligation_id = h
        .funds
        .find_obligation_for_transaction(txn_id)
        .await
        .unwrap()
        .unwrap()
        .id;

    // Two installments -> paid.
    for _ in 0..2 {
        h.fund_service
            .post_installment(obligation_id, jan.id, earmark, ymd(2026, 1, 20), now_ts())
            .await
            .expect("installment");
    }
    // A third must error and not mutate.
    let balance_before = h.fund_balance(fund_id).await;
    let result = h
        .fund_service
        .post_installment(obligation_id, jan.id, earmark, ymd(2026, 1, 20), now_ts())
        .await;
    assert!(
        result.is_err(),
        "posting against a paid obligation must be rejected"
    );
    assert_eq!(
        h.fund_balance(fund_id).await,
        balance_before,
        "a rejected installment must not move the buffer"
    );
}

// ===========================================================================
// SURPLUS DRAW (D7) — fund-draw, no repayment, no double-charge
// ===========================================================================

/// D7 surplus draw: drawing a surplus fund for a purchase is a fund-draw — it
/// creates NO obligation, draws the fund by the price, and is NOT re-charged as a
/// budget expense (excluded from the rolling net, so it is not double-counted
/// against the contributions that funded it).
#[tokio::test]
async fn surplus_draw_no_obligation_no_recharge_no_double_count() {
    let h = harness_zero_expected();
    // The earmark category for the surplus draw: a fund category so the draw is
    // excluded from the rolling net (BUDGET-NO-DOUBLE-CHARGE-1).
    let earmark = h.add_fund_category(12, Money::from_major(1_200));
    let fund_id = h.push_surplus(Money::from_major(1_000));

    let jan = h
        .lifecycle
        .ensure_current_month(h.user_id, ny_noon(2026, 1, 8))
        .await
        .expect("init jan");

    let price = Money::from_major(800);
    let txn_id = h
        .fund_service
        .record_large_purchase(
            h.user_id,
            jan.id,
            earmark,
            price,
            "Sofa".to_owned(),
            ymd(2026, 1, 12),
            LargePurchaseResolution::PayThroughSurplus(fund_id),
            now_ts(),
        )
        .await
        .expect("surplus draw");

    // Fund drawn by the price.
    assert_eq!(
        h.fund_balance(fund_id).await,
        Money::from_major(1_000) - price,
        "surplus fund drawn by the purchase price"
    );

    // NO obligation created.
    assert!(
        h.funds
            .find_obligation_for_transaction(txn_id)
            .await
            .unwrap()
            .is_none(),
        "a surplus draw creates NO repayment obligation"
    );
    assert!(
        h.funds
            .list_active_obligations(h.user_id)
            .await
            .unwrap()
            .is_empty(),
        "no active obligations after a surplus draw"
    );

    // NOT re-charged as a budget expense: the rolling net is 0 (the draw is
    // excluded), so it is not double-counted against the contribution that funded
    // the surplus earlier.
    h.lifecycle
        .ensure_current_month(h.user_id, ny_noon(2026, 2, 8))
        .await
        .expect("init feb");
    let feb_rollover = h.rollover_of(2026, 2).await;
    assert_eq!(
        feb_rollover,
        Money::ZERO,
        "a surplus draw is NOT re-charged as a budget expense (excluded from net)"
    );

    let jan_txns = h.txns.list_for_month(jan.id).await.unwrap();
    let oracle = oracle_month_net(&jan_txns, &[earmark], &[], Decimal::ZERO);
    assert_eq!(oracle, Decimal::ZERO);
    assert_eq!(feb_rollover.as_decimal(), oracle);
}

/// D7 no-double-count: the FULL arc — contribute into a surplus fund, then draw
/// it for a purchase — moves the dollar exactly once. Net over the month is
/// −0 (both the contribution and the draw are excluded), and the surplus fund
/// ends with (contributions − draw). No money created or destroyed.
#[tokio::test]
async fn surplus_contribute_then_draw_counts_dollar_once() {
    let h = harness_zero_expected();
    let earmark = h.add_fund_category(12, Money::from_major(1_200));
    let fund_id = h.push_surplus(Money::ZERO);

    let jan = h
        .lifecycle
        .ensure_current_month(h.user_id, ny_noon(2026, 1, 8))
        .await
        .expect("init jan");

    // Contribute $1,000 across two months-worth... here a single $1,000 earmark.
    let contribution = Money::from_major(1_000);
    h.fund_service
        .contribute(
            fund_id,
            jan.id,
            earmark,
            contribution,
            ymd(2026, 1, 5),
            now_ts(),
        )
        .await
        .expect("contribute");
    // Draw $800 for a purchase.
    let price = Money::from_major(800);
    h.fund_service
        .record_large_purchase(
            h.user_id,
            jan.id,
            earmark,
            price,
            "Desk".to_owned(),
            ymd(2026, 1, 20),
            LargePurchaseResolution::PayThroughSurplus(fund_id),
            now_ts(),
        )
        .await
        .expect("surplus draw");

    // Fund ends at contribution − draw.
    assert_eq!(h.fund_balance(fund_id).await, contribution - price);

    h.lifecycle
        .ensure_current_month(h.user_id, ny_noon(2026, 2, 8))
        .await
        .expect("init feb");

    // Both the contribution and the draw are excluded -> net 0.
    assert_eq!(h.rollover_of(2026, 2).await, Money::ZERO);

    // Oracle: contribution + draw both fund-category => excluded => net 0.
    let jan_txns = h.txns.list_for_month(jan.id).await.unwrap();
    let oracle = oracle_month_net(&jan_txns, &[earmark], &[], Decimal::ZERO);
    assert_eq!(oracle, Decimal::ZERO);
}

/// A surplus draw must be REJECTED on a buffer fund (the resolution requires
/// `compulsory_repayment` = false) — a buffer draw must go through buffer-finance.
#[tokio::test]
async fn surplus_draw_rejected_on_buffer_fund() {
    let h = harness_zero_expected();
    let earmark = h.add_fund_category(12, Money::from_major(1_200));
    let buffer = h.push_buffer(Money::from_major(5_000), Money::from_major(5_000));

    let jan = h
        .lifecycle
        .ensure_current_month(h.user_id, ny_noon(2026, 1, 8))
        .await
        .expect("init jan");

    let before = h.fund_balance(buffer).await;
    let result = h
        .fund_service
        .record_large_purchase(
            h.user_id,
            jan.id,
            earmark,
            Money::from_major(100),
            "X".to_owned(),
            ymd(2026, 1, 12),
            LargePurchaseResolution::PayThroughSurplus(buffer),
            now_ts(),
        )
        .await;
    assert!(
        result.is_err(),
        "pay_through_surplus on a buffer must error"
    );
    assert_eq!(
        h.fund_balance(buffer).await,
        before,
        "a rejected surplus draw must not move the buffer"
    );
}

// ===========================================================================
// SINKING FUND (SPEC §4.7)
// ===========================================================================

/// §4.7 accrual: each monthly accrual adds `amount / period_months` to the
/// category `fund_balance` and CARRIES OVER (does not reset). Three accruals on a
/// $1,200/12 fund leave `fund_balance` = $300, and each accrual posts an excluded
/// fund expense so the rolling net stays 0.
#[tokio::test]
async fn sinking_accrual_carries_over_and_is_excluded() {
    let h = harness_zero_expected();
    // $1,200 / 12 = $100 / month.
    let sinking = h.add_fund_category(12, Money::from_major(1_200));

    let jan = h
        .lifecycle
        .ensure_current_month(h.user_id, ny_noon(2026, 1, 8))
        .await
        .expect("init jan");

    let per_month = Money::from_major(100);

    // Accrue three times (simulating three months) into the SAME category — the
    // envelope must carry over, not reset.
    for _ in 0..3 {
        h.fund_service
            .accrue_sinking_fund(sinking, jan.id, h.user_id, ymd(2026, 1, 1), now_ts())
            .await
            .expect("accrue");
    }

    assert_eq!(
        h.category_fund_balance(sinking).await,
        per_month + per_month + per_month,
        "three accruals carry over to $300 (no reset)"
    );

    // Each accrual is a fund-category expense excluded from the net.
    h.lifecycle
        .ensure_current_month(h.user_id, ny_noon(2026, 2, 8))
        .await
        .expect("init feb");
    assert_eq!(
        h.rollover_of(2026, 2).await,
        Money::ZERO,
        "sinking accruals are excluded from the rolling net (BUDGET-FUND-EARMARK-1)"
    );

    let jan_txns = h.txns.list_for_month(jan.id).await.unwrap();
    let oracle = oracle_month_net(&jan_txns, &[sinking], &[], Decimal::ZERO);
    assert_eq!(oracle, Decimal::ZERO);
    assert_eq!(h.rollover_of(2026, 2).await.as_decimal(), oracle);
}

/// §4.7 accrual property: the carried-over balance equals exactly
/// `n * (amount / period_months)` after n accruals, for a spread of awkward
/// amounts/periods — no drift, no reset.
#[tokio::test]
async fn sinking_accrual_balance_is_exact_multiple_property() {
    for seed in 0..200_u64 {
        let mut rng = SplitMix64(seed.wrapping_mul(0xA076_1D64_78BD_642F).wrapping_add(11));
        let h = harness_zero_expected();

        let amount_cents = rng.cents(100, 500_000); // $1..$5000
        let period = i32::try_from((rng.next_u64() % 23) + 2).unwrap_or(2); // 2..24
        let sinking = h.add_fund_category(period, Money::from_minor(amount_cents));

        let jan = h
            .lifecycle
            .ensure_current_month(h.user_id, ny_noon(2026, 1, 8))
            .await
            .expect("init jan");

        // Per-month accrual = round(amount / period) to cents (independent oracle).
        let per = (Decimal::new(amount_cents, 2) / Decimal::from(period)).round_dp(2);

        let n = (rng.next_u64() % 6) + 1;
        for _ in 0..n {
            h.fund_service
                .accrue_sinking_fund(sinking, jan.id, h.user_id, ymd(2026, 1, 1), now_ts())
                .await
                .expect("accrue");
        }

        let expected = per * Decimal::from(n);
        assert_eq!(
            h.category_fund_balance(sinking).await.as_decimal(),
            expected,
            "seed {seed}: amount={amount_cents} period={period} n={n}: \
             carried-over balance must be n * accrual, no drift/reset"
        );
    }
}

/// §4.7 reset-on-payment: tagging a real bill as the payout draws the reserve
/// down by the paid amount (to/below zero if under-saved) and re-anchors the
/// accrual clock FORWARD by one full period from the PAYMENT date (forward-
/// looking accrual, not backward).
#[tokio::test]
async fn sinking_payout_draws_reserve_and_anchors_clock_forward() {
    let h = harness_zero_expected();
    // Quarterly $300/3 = $100/month.
    let sinking = h.add_fund_category(3, Money::from_major(300));

    let jan = h
        .lifecycle
        .ensure_current_month(h.user_id, ny_noon(2026, 1, 8))
        .await
        .expect("init jan");

    // Accrue three months => reserve $300.
    for _ in 0..3 {
        h.fund_service
            .accrue_sinking_fund(sinking, jan.id, h.user_id, ymd(2026, 1, 1), now_ts())
            .await
            .expect("accrue");
    }
    assert_eq!(
        h.category_fund_balance(sinking).await,
        Money::from_major(300)
    );

    // A real bill lands: $290, tagged as the payout, paid 2026-03-15.
    let bill = expense_txn(&h, jan.id, sinking, Money::from_major(-290));
    let bill_id = bill.id;
    h.txns.push(bill);

    let payment_date = ymd(2026, 3, 15);
    let updated = h
        .fund_service
        .tag_sinking_payout(
            sinking,
            bill_id,
            Money::from_major(290),
            payment_date,
            now_ts(),
        )
        .await
        .expect("tag payout");

    // Reserve drawn down by the paid amount: $300 − $290 = $10.
    assert_eq!(
        updated.fund_balance,
        Money::from_major(10),
        "reserve drawn down by the paid amount"
    );

    // Clock re-anchored FORWARD by one full period (3 months) from the PAYMENT
    // date: 2026-03-15 + 3 months = 2026-06-15.
    assert_eq!(
        updated.next_due_date,
        Some(ymd(2026, 6, 15)),
        "next_due_date re-anchored forward one period from the payment date"
    );
}

/// §4.7 under-saved payout: if the bill exceeds the accrued reserve, the reserve
/// goes NEGATIVE (a shortfall the next accrual catches up) — to/below zero, never
/// clamped silently.
#[tokio::test]
async fn sinking_payout_under_saved_goes_negative() {
    let h = harness_zero_expected();
    let sinking = h.add_fund_category(3, Money::from_major(300));

    let jan = h
        .lifecycle
        .ensure_current_month(h.user_id, ny_noon(2026, 1, 8))
        .await
        .expect("init jan");

    // Only ONE accrual => reserve $100.
    h.fund_service
        .accrue_sinking_fund(sinking, jan.id, h.user_id, ymd(2026, 1, 1), now_ts())
        .await
        .expect("accrue");

    let bill = expense_txn(&h, jan.id, sinking, Money::from_major(-280));
    let bill_id = bill.id;
    h.txns.push(bill);

    let updated = h
        .fund_service
        .tag_sinking_payout(
            sinking,
            bill_id,
            Money::from_major(280),
            ymd(2026, 2, 10),
            now_ts(),
        )
        .await
        .expect("tag payout");

    // $100 reserve − $280 bill = −$180 (under-saved shortfall, surfaced not hidden).
    assert_eq!(
        updated.fund_balance,
        Money::from_major(-180),
        "an under-saved payout drives the reserve negative (shortfall surfaced)"
    );
}

// ===========================================================================
// BUFFER HEALTH — advisory only, never blocks
// ===========================================================================

/// Buffer health is informational: a draw that takes the buffer BELOW its
/// caution target still succeeds and mutates state. The health verdict reflects
/// the new state but never gated the draw.
#[tokio::test]
async fn buffer_health_is_advisory_and_never_blocks_a_draw() {
    let h = harness_zero_expected();
    let earmark = h.add_fund_category(12, Money::from_major(1_200));
    // Buffer at target $5,000.
    let target = Money::from_major(5_000);
    let fund_id = h.push_buffer(target, target);

    let jan = h
        .lifecycle
        .ensure_current_month(h.user_id, ny_noon(2026, 1, 8))
        .await
        .expect("init jan");

    // Before: on target.
    assert_eq!(
        h.fund_service.buffer_health_for(fund_id).await.unwrap(),
        BufferHealth::OnTarget
    );

    // A large buffer-financed draw that takes the balance well below target.
    let price = Money::from_major(4_000);
    h.fund_service
        .record_large_purchase(
            h.user_id,
            jan.id,
            earmark,
            price,
            "Big".to_owned(),
            ymd(2026, 1, 12),
            LargePurchaseResolution::BufferFinanced { fund_id, months: 6 },
            now_ts(),
        )
        .await
        .expect("draw succeeds despite below-target outcome");

    // The draw HAPPENED (not blocked): balance is 5000 - 4000 = 1000.
    assert_eq!(
        h.fund_balance(fund_id).await,
        Money::from_major(1_000),
        "the draw succeeds regardless of buffer health"
    );

    // The advisory now reports below-target-with-obligations — but it only
    // *reports*; it never prevented the draw above.
    let verdict = h.fund_service.buffer_health_for(fund_id).await.unwrap();
    assert_eq!(
        verdict,
        BufferHealth::BelowTargetWithObligations(target - Money::from_major(1_000)),
        "health reflects the new (post-draw) state, advisory only"
    );
}

/// Buffer health verdicts on the pure function: above/below/on-target, and a
/// surplus fund (no target) is always neutral — never a draw gate.
#[tokio::test]
async fn buffer_health_verdicts_are_pure_and_neutral_for_non_buffer() {
    let h = harness_zero_expected();
    let target = Money::from_major(5_000);

    // Above target -> AboveTarget(excess), regardless of obligations.
    let above_id = h.push_buffer(Money::from_major(6_000), target);
    let above = h.funds.find_by_id(above_id).await.unwrap().unwrap();
    assert_eq!(
        FundService::buffer_health(&above, false),
        BufferHealth::AboveTarget(Money::from_major(1_000))
    );
    assert_eq!(
        FundService::buffer_health(&above, true),
        BufferHealth::AboveTarget(Money::from_major(1_000)),
        "above-target verdict ignores obligations"
    );

    // Below target: with obligations -> caution; without -> plain below.
    let below_id = h.push_buffer(Money::from_major(4_000), target);
    let below = h.funds.find_by_id(below_id).await.unwrap().unwrap();
    assert_eq!(
        FundService::buffer_health(&below, true),
        BufferHealth::BelowTargetWithObligations(Money::from_major(1_000))
    );
    assert_eq!(
        FundService::buffer_health(&below, false),
        BufferHealth::BelowTarget(Money::from_major(1_000))
    );

    // On target -> OnTarget.
    let on_id = h.push_buffer(target, target);
    let on = h.funds.find_by_id(on_id).await.unwrap().unwrap();
    assert_eq!(
        FundService::buffer_health(&on, false),
        BufferHealth::OnTarget
    );

    // A surplus fund (no target) is always neutral.
    let surplus_id = h.push_surplus(Money::from_major(9_999));
    let surplus = h.funds.find_by_id(surplus_id).await.unwrap().unwrap();
    assert_eq!(
        FundService::buffer_health(&surplus, true),
        BufferHealth::OnTarget,
        "a non-buffer fund is always neutral (no health gate)"
    );
}

// ===========================================================================
// Smoke: the two services share a store, so cross-service state is consistent.
// ===========================================================================

/// Sanity: contributions and accruals add transactions to the SAME store the
/// lifecycle reads, so a mixed month (income + ordinary expense + fund earmark)
/// nets exactly the non-fund part. Guards against the fakes silently diverging.
#[tokio::test]
async fn mixed_month_nets_only_the_non_fund_part() {
    let h = harness_with_expected(Money::from_major(4_000));
    let exp_cat = h.add_expense_category();
    let fund_cat = h.add_fund_category(12, Money::from_major(1_200));
    let fund_id = h.push_surplus(Money::ZERO);

    let jan = h
        .lifecycle
        .ensure_current_month(h.user_id, ny_noon(2026, 1, 8))
        .await
        .expect("init jan");

    // Income $4,050 (=> +$50 surplus over the $4,000 expectation).
    let mut income = expense_txn(&h, jan.id, exp_cat, Money::from_major(4_050));
    income.category_id = None;
    income.income_kind = Some(IncomeKind::Budgeted);
    h.txns.push(income);
    // Ordinary expense −$30.
    h.txns
        .push(expense_txn(&h, jan.id, exp_cat, Money::from_major(-30)));
    // Fund contribution $200 (excluded).
    h.fund_service
        .contribute(
            fund_id,
            jan.id,
            fund_cat,
            Money::from_major(200),
            ymd(2026, 1, 9),
            now_ts(),
        )
        .await
        .expect("contribute");

    let before_txns = h.txns.count();
    assert!(before_txns >= 3, "income + expense + contribution present");

    h.lifecycle
        .ensure_current_month(h.user_id, ny_noon(2026, 2, 8))
        .await
        .expect("init feb");

    // Net = (4050 - 4000) + (-30) = +$20. The $200 fund earmark is excluded.
    assert_eq!(
        h.rollover_of(2026, 2).await,
        Money::from_major(20),
        "net counts only the non-fund part: (+50 income) + (-30 expense) = +20"
    );

    let jan_txns = h.txns.list_for_month(jan.id).await.unwrap();
    let oracle = oracle_month_net(&jan_txns, &[fund_cat], &[], Decimal::new(400_000, 2));
    assert_eq!(oracle, Decimal::new(2_000, 2));
    assert_eq!(h.rollover_of(2026, 2).await.as_decimal(), oracle);
}
