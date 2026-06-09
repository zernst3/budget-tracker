//! Independent property + oracle tests for the **income engine** (build step 6;
//! `SPEC §4.8`, D5 income-variance term).
//!
//! These tests were authored by a SEPARATE test-author agent that did **not**
//! trust the build's own `src/income/tests.rs` (`ORCH-REVIEWER-SPLIT-1` spirit).
//! They build their own in-memory fakes from scratch against the crate's *public*
//! trait surface only (never importing the build's test fakes) and carry their
//! own `rust_decimal` oracle that re-derives the expected-income figures and D5
//! net independently of the production `ConfigDrivenIncomeExpectation` and
//! `net_leftover`. Where the production formula is cross-checked, that is an
//! additional assertion, never the sole oracle.
//!
//! ## Invariants covered (`ORCH-NEW-PATH-TESTS-1`, `PROC-REGRESSION-TEST-1`)
//!
//! **(a) Semimonthly oracle.**
//!   For every calendar month, `expected_income` for a semimonthly / per-paycheck
//!   config equals exactly `2 × amount` to the cent, verified against an
//!   independent `Decimal` oracle (`2 * Decimal::from(amount_cents) / 100`).
//!   End-to-end: a paycheck above the expectation moves the D5 net (and thus the
//!   Feb rollover) by exactly the variance — verified via the month-lifecycle
//!   service wired over independent fakes, with the Oracle cross-checked on the
//!   raw `Decimal` layer.
//!
//! **(b) Hourly / variable (blank amount).**
//!   For every (mode, cadence) combination, a `None` amount yields an expected
//!   income of `Money::ZERO`. Wired into D5: actual income flows straight into
//!   Other — net = actual, no subtraction — verified to the cent against an
//!   independent oracle.
//!
//! **(c) Surplus routing.**
//!   - `this_month`: no fund contribution posted; Other rises by the surplus (the
//!     D5 formula already does it — no extra transaction).
//!   - `buffer` / `savings`: surplus becomes a `FundService::contribute` into the
//!     target fund; the fund balance rises by exactly the surplus and exactly ONE
//!     transaction is posted; the dollar is counted once (`BUDGET-FUND-EARMARK-1`).
//!   - Per-transaction override: when an override is supplied, it wins over the
//!     config default regardless of direction (default=buffer, override=`this_month`
//!     and vice versa).
//!   - Property: the override-wins invariant holds across 1 000 seeded random
//!     (default, override) pairs.
//!
//! **(d) Stubbed paths fail loudly-but-safely.**
//!   Every unbuilt mode × cadence combination with a fixed amount returns
//!   `ServiceError::UnsupportedIncome`; NONE panic or silently return a wrong
//!   figure. Exhaustive enumeration of the 6 unbuilt arms + a property loop over
//!   random amounts confirms the error is stable (not amount-dependent).
//!
//! Property tests use a deterministic splitmix64 PRNG (no crate dependency; same
//! approach as the existing independent test files in this crate). The seed prints
//! on failure so a counterexample replays exactly.
//!
//! ### Lint suppressions (test-only)
//!
//! The workspace denies `unwrap_used`, `expect_used`, and `panic` in production
//! code. Test code panics on assertion failure by design; the bans are suppressed
//! for this integration test only, matching the in-crate convention.

#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]
#![allow(clippy::panic)]
#![allow(clippy::too_many_lines)]
#![allow(clippy::too_many_arguments)]
#![allow(clippy::cast_possible_truncation)]

use std::any::Any;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use chrono_tz::America::New_York;
use rust_decimal::Decimal;

use budget_app_services::income::{
    ConfigDrivenIncomeExpectation, FixedExpectation, IncomeExpectation, IncomeSurplusRouter,
    SemimonthlyFixedExpectation,
};
use budget_app_services::{FundService, MonthLifecycleService, ServiceError};

use budget_domain::budget::Budget;
use budget_domain::category::Category;
use budget_domain::enums::{
    Cadence, CategoryGrp, FundKind, IncomeKind, IncomeMode, ObligationSource, ObligationStatus,
    PaycheckType, SurplusRouting, TransactionSource, TransactionStatus,
};
use budget_domain::fund::Fund;
use budget_domain::ids::{
    BudgetId, CategoryId, CategoryKey, FundId, MonthId, PaycheckConfigId, RepaymentObligationId,
    TransactionId, UserId,
};
use budget_domain::money::Money;
use budget_domain::month::Month;
use budget_domain::paycheck_config::PaycheckConfig;
use budget_domain::repayment_obligation::RepaymentObligation;
use budget_domain::repositories::{
    BudgetRepository, FundRepository, PaycheckConfigRepository, TransactionRepository,
};
use budget_domain::transaction::Transaction;
use budget_domain::uow::{UnitOfWork, UowFuture, UowProvider};
use budget_domain::{CategorySpent, MonthNet, MonthRepository, RepositoryError};

// ===========================================================================
// Deterministic PRNG (splitmix64) — same approach as other independent tests.
// ===========================================================================

struct SplitMix64(u64);

impl SplitMix64 {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A cents value in `[lo, hi]`.
    fn cents(&mut self, lo: i64, hi: i64) -> i64 {
        let span = (hi - lo).unsigned_abs() + 1;
        let raw = i64::try_from(self.next_u64() % span).unwrap_or(0);
        lo + raw
    }

    fn bool(&mut self) -> bool {
        self.next_u64() & 1 == 1
    }
}

// ===========================================================================
// Independent in-memory fakes
// (built ONLY against the public trait surface — never importing build fakes)
// ===========================================================================

struct NoopUow;
impl UnitOfWork for NoopUow {
    fn as_any(&self) -> &dyn Any {
        self
    }
}

type BoxedClosure<'a> =
    Box<dyn for<'u> FnOnce(&'u dyn UnitOfWork) -> UowFuture<'u, Box<dyn Any + Send>> + Send + 'a>;

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

// ---- PaycheckConfigRepository fake -----------------------------------------

/// Simple in-memory paycheck config store keyed by `user_id`.
#[derive(Default)]
struct MemPaycheckConfigRepo {
    configs: Mutex<Vec<PaycheckConfig>>,
}

impl MemPaycheckConfigRepo {
    fn with(config: PaycheckConfig) -> Self {
        Self {
            configs: Mutex::new(vec![config]),
        }
    }
}

#[async_trait]
impl PaycheckConfigRepository for MemPaycheckConfigRepo {
    async fn find_for_user(
        &self,
        user_id: UserId,
    ) -> Result<Option<PaycheckConfig>, RepositoryError> {
        let g = self.configs.lock().map_err(poisoned)?;
        Ok(g.iter().find(|c| c.user_id == user_id).cloned())
    }

    async fn save(
        &self,
        config: &PaycheckConfig,
        _uow: Option<&dyn UnitOfWork>,
    ) -> Result<(), RepositoryError> {
        let mut g = self.configs.lock().map_err(poisoned)?;
        if let Some(slot) = g.iter_mut().find(|c| c.user_id == config.user_id) {
            *slot = config.clone();
        } else {
            g.push(config.clone());
        }
        Ok(())
    }
}

// ---- FundRepository fake ----------------------------------------------------

#[derive(Default)]
struct MemFundRepo {
    funds: Mutex<Vec<Fund>>,
    obligations: Mutex<Vec<RepaymentObligation>>,
}

impl MemFundRepo {
    fn with_fund(fund: Fund) -> Self {
        Self {
            funds: Mutex::new(vec![fund]),
            obligations: Mutex::new(vec![]),
        }
    }
}

#[async_trait]
impl FundRepository for MemFundRepo {
    async fn find_by_id(&self, id: FundId) -> Result<Option<Fund>, RepositoryError> {
        let g = self.funds.lock().map_err(poisoned)?;
        Ok(g.iter().find(|f| f.id == id).cloned())
    }

    async fn list_for_user(&self, user_id: UserId) -> Result<Vec<Fund>, RepositoryError> {
        let g = self.funds.lock().map_err(poisoned)?;
        Ok(g.iter().filter(|f| f.user_id == user_id).cloned().collect())
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
        let g = self.obligations.lock().map_err(poisoned)?;
        Ok(g.iter().find(|o| o.id == id).cloned())
    }

    async fn list_active_obligations(
        &self,
        user_id: UserId,
    ) -> Result<Vec<RepaymentObligation>, RepositoryError> {
        let g = self.obligations.lock().map_err(poisoned)?;
        Ok(g.iter().filter(|o| o.user_id == user_id).cloned().collect())
    }

    async fn find_obligation_for_transaction(
        &self,
        transaction_id: TransactionId,
    ) -> Result<Option<RepaymentObligation>, RepositoryError> {
        let g = self.obligations.lock().map_err(poisoned)?;
        Ok(g.iter()
            .find(|o| o.transaction_id == Some(transaction_id))
            .cloned())
    }

    async fn find_active_deficit_obligation_for_month(
        &self,
        month_id: MonthId,
    ) -> Result<Option<RepaymentObligation>, RepositoryError> {
        let g = self.obligations.lock().map_err(poisoned)?;
        Ok(g.iter()
            .find(|o| {
                o.origin_month_id == Some(month_id)
                    && o.source == ObligationSource::Deficit
                    && o.status == ObligationStatus::Active
            })
            .cloned())
    }

    async fn list_buffer_financed_transaction_ids(
        &self,
        user_id: UserId,
    ) -> Result<Vec<TransactionId>, RepositoryError> {
        let g = self.obligations.lock().map_err(poisoned)?;
        Ok(g.iter()
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

// ---- TransactionRepository fake --------------------------------------------

#[derive(Default)]
struct MemTxnRepo {
    txns: Mutex<Vec<Transaction>>,
}

impl MemTxnRepo {
    fn push(&self, t: Transaction) {
        self.txns.lock().unwrap().push(t);
    }

    fn all(&self) -> Vec<Transaction> {
        self.txns.lock().unwrap().clone()
    }

    fn for_month(&self, month_id: MonthId) -> Vec<Transaction> {
        self.txns
            .lock()
            .unwrap()
            .iter()
            .filter(|t| t.month_id == month_id)
            .cloned()
            .collect()
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
        plaid_id: &str,
    ) -> Result<Option<Transaction>, RepositoryError> {
        let g = self.txns.lock().map_err(poisoned)?;
        Ok(g.iter()
            .find(|t| t.plaid_transaction_id.as_deref() == Some(plaid_id))
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
        // Independent inclusion: settled + expected count; pending excluded.
        let g = self.txns.lock().map_err(poisoned)?;
        let net: Money = g
            .iter()
            .filter(|t| {
                t.month_id == month_id
                    && matches!(
                        t.status,
                        TransactionStatus::Settled | TransactionStatus::Expected
                    )
            })
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
        // Partial-unique guard: reject a second rollover for the same month.
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

// ---- BudgetRepository fake -------------------------------------------------

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

// ---- MonthRepository fake --------------------------------------------------

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

// ===========================================================================
// Independent oracle helpers
// ===========================================================================

/// **Independent oracle for `expected_income`:** semimonthly is always
/// exactly 2 paychecks/month (`SPEC §4.8`), so the expected income is
/// `2 × per_paycheck_cents / 100` expressed as a `Decimal`.
///
/// Re-derived without any reference to production code so a bug in
/// `semimonthly_expected` would not hide itself in the oracle.
fn oracle_semimonthly_expected_cents(per_paycheck_cents: i64) -> Decimal {
    Decimal::from(per_paycheck_cents) * Decimal::from(2_i32) / Decimal::from(100_i32)
}

/// **Independent oracle for D5 net.** Re-derives:
/// ```text
/// net = (actual_income − expected_income) + Σ(expense_remainings)
/// ```
/// entirely in `Decimal`, with no reference to the production `net_leftover`
/// or any production predicate. Income rows are those with `income_kind` set.
/// Rollover rows, fund-draw rows, and pending rows are excluded by the same
/// independent logic used in the other independent test files.
fn oracle_d5_net(txns: &[Transaction], expected_income_cents: i64) -> Decimal {
    let expected = Decimal::from(expected_income_cents) / Decimal::from(100_i32);
    let mut actual = Decimal::ZERO;
    let mut expense_remaining = Decimal::ZERO;
    for t in txns {
        // Pending is excluded independently (BUDGET-STATUS-DRIVES-INCLUSION-1).
        if t.status == TransactionStatus::Pending {
            continue;
        }
        let amt = t.amount.as_decimal();
        if t.income_kind.is_some() {
            actual += amt;
        } else if t.is_fund_draw {
            // Fund draw excluded (already expensed at contribution — D6 Model A).
        } else {
            expense_remaining += amt;
        }
    }
    (actual - expected) + expense_remaining
}

// ===========================================================================
// Shared helpers
// ===========================================================================

fn ymd(y: i32, m: u32, d: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, d).expect("valid date")
}

fn ny_noon(year: i32, month: u32, day: u32) -> DateTime<Utc> {
    let naive = ymd(year, month, day).and_hms_opt(12, 0, 0).expect("noon");
    New_York
        .from_local_datetime(&naive)
        .single()
        .expect("unambiguous NY local time")
        .with_timezone(&Utc)
}

/// Build a `PaycheckConfig` for a user.
fn config(
    user_id: UserId,
    mode: IncomeMode,
    ptype: PaycheckType,
    amount: Option<Money>,
) -> PaycheckConfig {
    PaycheckConfig {
        id: PaycheckConfigId::generate(),
        user_id,
        income_mode: mode,
        paycheck_type: ptype,
        amount,
        anchor_date: ymd(2026, 6, 1),
        surplus_routing: SurplusRouting::Buffer,
        smoothing_buffer: Money::ZERO,
    }
}

/// Build a minimal budget + rollover bucket; return `(budget_id, rollover_bucket_id)`.
fn seed_budget(budgets: &MemBudgetRepo, user_id: UserId) -> (BudgetId, CategoryId) {
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
    (budget_id, rollover_bucket_id)
}

fn base_txn(user_id: UserId, month_id: MonthId, amount: Money) -> Transaction {
    Transaction {
        id: TransactionId::generate(),
        user_id,
        month_id,
        category_id: None,
        account_id: None,
        date: ymd(2026, 6, 15),
        amount,
        description: "t".to_owned(),
        source: TransactionSource::Manual,
        plaid_transaction_id: None,
        status: TransactionStatus::Settled,
        income_kind: None,
        is_rollover: false,
        is_fund_draw: false,
        matched_transaction_id: None,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    }
}

// ===========================================================================
// (a) Semimonthly expected = 2 × amount — oracle verified, all 12 months
// ===========================================================================

/// `ConfigDrivenIncomeExpectation` (loaded via repository) returns exactly
/// `2 × per_paycheck_amount` for every calendar month, verified against the
/// independent `oracle_semimonthly_expected_cents`.
#[tokio::test]
async fn semimonthly_expected_equals_two_times_amount_for_every_month_oracle_verified() {
    // Representative amounts: whole dollars, odd cents, large value.
    let cases: &[i64] = &[
        200_000, // $2,000.00
        245_099, // $2,450.99
        175_001, // $1,750.01 — one cent above a round number
        100_000, // $1,000.00
        333_333, // $3,333.33 — the classic awkward-thirds figure
        999_999, // $9,999.99
        1,       // $0.01 — one cent
    ];

    for &per_paycheck_cents in cases {
        let user_id = UserId::generate();
        let cfg = config(
            user_id,
            IncomeMode::PerPaycheck,
            PaycheckType::Semimonthly,
            Some(Money::from_minor(per_paycheck_cents)),
        );
        let repo = MemPaycheckConfigRepo::with(cfg.clone());
        let exp = ConfigDrivenIncomeExpectation::load(&repo, user_id)
            .await
            .expect("semimonthly loads");

        let oracle_dollars = oracle_semimonthly_expected_cents(per_paycheck_cents);

        for month in 1_i32..=12 {
            let result = exp.expected_income(user_id, 2026, month);
            assert_eq!(
                result.as_decimal(),
                oracle_dollars,
                "per_paycheck_cents={per_paycheck_cents} month={month}: \
                 expected_income must equal oracle (2 × amount)"
            );
            // Also cross-check that the `from_config` fast-path agrees.
            let exp2 = ConfigDrivenIncomeExpectation::from_config(&cfg).expect("from_config");
            assert_eq!(
                exp2.expected_income(user_id, 2026, month),
                result,
                "load() and from_config() must agree for month={month}"
            );
        }

        // `SemimonthlyFixedExpectation` (the lightweight alias) must also agree.
        let simple = SemimonthlyFixedExpectation::new(Money::from_minor(per_paycheck_cents));
        for month in 1_i32..=12 {
            assert_eq!(
                simple.expected_income(user_id, 2026, month).as_decimal(),
                oracle_dollars,
                "SemimonthlyFixedExpectation must match oracle for month={month}"
            );
        }
    }
}

/// Property: for 500 random per-paycheck amounts, the `expected_income` from
/// `ConfigDrivenIncomeExpectation` equals exactly `2 × amount` against the
/// oracle, with zero cent drift.
#[tokio::test]
async fn semimonthly_expected_property_two_times_amount_zero_cent_drift() {
    let mut rng = SplitMix64::new(0xABCD_EF01_2345_6789);

    for seed_idx in 0..500_u64 {
        let per_paycheck_cents = rng.cents(1, 1_000_000); // $0.01 to $10,000.00
        let user_id = UserId::generate();
        let cfg = config(
            user_id,
            IncomeMode::PerPaycheck,
            PaycheckType::Semimonthly,
            Some(Money::from_minor(per_paycheck_cents)),
        );
        let exp = ConfigDrivenIncomeExpectation::from_config(&cfg).expect("built");
        let oracle = oracle_semimonthly_expected_cents(per_paycheck_cents);

        // Check in any arbitrary month — semimonthly is month-invariant.
        let month = i32::try_from((rng.next_u64() % 12) + 1).unwrap_or(1);
        assert_eq!(
            exp.expected_income(user_id, 2026, month).as_decimal(),
            oracle,
            "seed_idx={seed_idx} per_paycheck_cents={per_paycheck_cents}: \
             expected_income drifted from oracle"
        );
    }
}

/// End-to-end D5 oracle: a paycheck ABOVE expectation moves the Feb rollover
/// by exactly the variance amount; a paycheck BELOW expectation moves it by
/// exactly the negative variance. Both are verified against the independent
/// `oracle_d5_net`.
#[tokio::test]
async fn d5_net_income_variance_moves_other_by_exact_variance_e2e_oracle() {
    // (per_paycheck_cents, actual_income_cents, expected_rollover_cents)
    // rollover = (actual - 2×per_paycheck) + 0 expenses
    let cases: &[(i64, i64)] = &[
        (200_000, 410_000), // $500 surplus: expected $4000, actual $4100 = +$100
        (200_000, 390_000), // $-100 shortfall
        (245_099, 500_000), // awkward expected + surplus
        (100_000, 200_000), // exact match → zero rollover
        (100_000, 150_000), // below — $50 shortfall
        (333_333, 700_000), // awkward cents: expected $6666.66, actual $7000 = +$333.34
    ];

    for &(per_paycheck_cents, actual_cents) in cases {
        let user_id = UserId::generate();
        let months = Arc::new(MemMonthRepo::default());
        let budgets = Arc::new(MemBudgetRepo::default());
        let txns = Arc::new(MemTxnRepo::default());
        let funds = Arc::new(MemFundRepo::default());

        let (_, rollover_bucket_id) = seed_budget(&budgets, user_id);

        let income_exp = Arc::new(
            ConfigDrivenIncomeExpectation::from_config(&config(
                user_id,
                IncomeMode::PerPaycheck,
                PaycheckType::Semimonthly,
                Some(Money::from_minor(per_paycheck_cents)),
            ))
            .expect("built"),
        );

        let svc = MonthLifecycleService::new(
            Arc::clone(&months) as _,
            Arc::clone(&budgets) as _,
            Arc::clone(&txns) as _,
            Arc::clone(&funds) as _,
            Arc::new(NoopUowProvider) as _,
            income_exp,
        );

        let jan = svc
            .ensure_current_month(user_id, ny_noon(2026, 1, 8))
            .await
            .expect("init jan");

        // Post the actual paycheck.
        let mut income_txn = base_txn(user_id, jan.id, Money::from_minor(actual_cents));
        income_txn.income_kind = Some(IncomeKind::Budgeted);
        txns.push(income_txn);

        // Advance to Feb — the service posts Jan's rollover.
        svc.ensure_current_month(user_id, ny_noon(2026, 2, 8))
            .await
            .expect("init feb");

        let feb = months
            .find_by_year_month(user_id, 2026, 2)
            .await
            .unwrap()
            .expect("feb exists");
        let rollover = txns
            .find_rollover_for_month(feb.id)
            .await
            .unwrap()
            .expect("feb rollover exists");

        // Independent oracle on the raw Decimal layer.
        let jan_txns = txns.for_month(jan.id);
        let oracle = oracle_d5_net(&jan_txns, per_paycheck_cents * 2);

        assert_eq!(
            rollover.amount.as_decimal(),
            oracle,
            "per_paycheck_cents={per_paycheck_cents} actual_cents={actual_cents}: \
             rollover must equal independent D5 oracle"
        );
        // Structural checks.
        assert_eq!(rollover.category_id, Some(rollover_bucket_id));
        assert!(rollover.is_rollover);
        assert_eq!(rollover.date, ymd(2026, 2, 1));
    }
}

/// Property: across 200 seeded (`per_paycheck`, actual) pairs, the Feb rollover
/// always equals the independent D5 oracle — zero cent drift across all amounts.
#[tokio::test]
async fn d5_income_variance_property_zero_drift_vs_oracle() {
    let mut rng = SplitMix64::new(0x1234_5678_DEAD_BEEF);

    for seed_idx in 0..200_u64 {
        let per_paycheck_cents = rng.cents(50_000, 500_000); // $500–$5000 per paycheck
        let actual_cents = rng.cents(per_paycheck_cents - 50_000, per_paycheck_cents + 50_000);

        let user_id = UserId::generate();
        let months = Arc::new(MemMonthRepo::default());
        let budgets = Arc::new(MemBudgetRepo::default());
        let txns = Arc::new(MemTxnRepo::default());
        let funds = Arc::new(MemFundRepo::default());
        seed_budget(&budgets, user_id);

        let income_exp = Arc::new(
            ConfigDrivenIncomeExpectation::from_config(&config(
                user_id,
                IncomeMode::PerPaycheck,
                PaycheckType::Semimonthly,
                Some(Money::from_minor(per_paycheck_cents)),
            ))
            .expect("built"),
        );

        let svc = MonthLifecycleService::new(
            Arc::clone(&months) as _,
            Arc::clone(&budgets) as _,
            Arc::clone(&txns) as _,
            Arc::clone(&funds) as _,
            Arc::new(NoopUowProvider) as _,
            income_exp,
        );

        let jan = svc
            .ensure_current_month(user_id, ny_noon(2026, 1, 8))
            .await
            .expect("init jan");

        let mut income_txn = base_txn(user_id, jan.id, Money::from_minor(actual_cents));
        income_txn.income_kind = Some(IncomeKind::Budgeted);
        txns.push(income_txn);

        svc.ensure_current_month(user_id, ny_noon(2026, 2, 8))
            .await
            .expect("init feb");

        let feb = months
            .find_by_year_month(user_id, 2026, 2)
            .await
            .unwrap()
            .expect("feb");
        let rollover = txns
            .find_rollover_for_month(feb.id)
            .await
            .unwrap()
            .expect("feb rollover");

        let jan_txns = txns.for_month(jan.id);
        let oracle = oracle_d5_net(&jan_txns, per_paycheck_cents * 2);

        assert_eq!(
            rollover.amount.as_decimal(),
            oracle,
            "seed_idx={seed_idx} per_paycheck_cents={per_paycheck_cents} \
             actual_cents={actual_cents}: rollover drifted from oracle"
        );
    }
}

// ===========================================================================
// (b) Hourly / variable (blank amount) => expected ZERO => actual flows into Other
// ===========================================================================

/// For every (mode, cadence) combination with `amount = None`, the expected
/// income is `Money::ZERO` and the D5 net equals the actual income (no
/// subtraction). Both `from_config` (sync) and `load` (async / repo-backed)
/// are verified.
#[tokio::test]
async fn blank_amount_expected_zero_for_all_mode_cadence_combos() {
    let modes = [IncomeMode::PerPaycheck, IncomeMode::Smoothed];
    let cadences = [
        PaycheckType::Semimonthly,
        PaycheckType::Biweekly,
        PaycheckType::Weekly,
        PaycheckType::Hourly,
    ];

    for mode in modes {
        for cadence in cadences {
            let user_id = UserId::generate();
            let cfg = config(user_id, mode, cadence, None);

            // from_config path.
            let built_from_cfg = ConfigDrivenIncomeExpectation::from_config(&cfg)
                .expect("blank amount is buildable for all combos");
            for month in 1_i32..=12 {
                assert_eq!(
                    built_from_cfg.expected_income(user_id, 2026, month),
                    Money::ZERO,
                    "blank amount must yield ZERO expected for {mode:?}/{cadence:?} \
                     month={month} (sync path)"
                );
            }

            // load path (via repo).
            let repo = MemPaycheckConfigRepo::with(cfg.clone());
            let built_via_load = ConfigDrivenIncomeExpectation::load(&repo, user_id)
                .await
                .expect("blank amount is buildable via load()");
            for month in 1_i32..=12 {
                assert_eq!(
                    built_via_load.expected_income(user_id, 2026, month),
                    Money::ZERO,
                    "blank amount must yield ZERO expected for {mode:?}/{cadence:?} \
                     month={month} (async/load path)"
                );
            }
        }
    }
}

/// End-to-end: with blank amount, `expected = ZERO`, so `D5 net = actual − 0 =
/// actual`. The actual paycheck flows straight into Other — verified to the cent
/// against `oracle_d5_net` with `expected_income_cents = 0`.
#[tokio::test]
async fn blank_amount_actual_flows_straight_into_other_e2e_oracle() {
    // (actual_cents) — the expected_income is forced to ZERO by the blank config.
    let cases: &[i64] = &[300_000, 100_001, 1, 999_999, 412_345];

    for &actual_cents in cases {
        let user_id = UserId::generate();
        let months = Arc::new(MemMonthRepo::default());
        let budgets = Arc::new(MemBudgetRepo::default());
        let txns = Arc::new(MemTxnRepo::default());
        let funds = Arc::new(MemFundRepo::default());

        let (_, rollover_bucket_id) = seed_budget(&budgets, user_id);

        // Hourly/variable: amount = None.
        let income_exp: Arc<dyn IncomeExpectation> = Arc::new(FixedExpectation::zero());

        let svc = MonthLifecycleService::new(
            Arc::clone(&months) as _,
            Arc::clone(&budgets) as _,
            Arc::clone(&txns) as _,
            Arc::clone(&funds) as _,
            Arc::new(NoopUowProvider) as _,
            income_exp,
        );

        let jan = svc
            .ensure_current_month(user_id, ny_noon(2026, 1, 8))
            .await
            .expect("init jan");

        let mut income_txn = base_txn(user_id, jan.id, Money::from_minor(actual_cents));
        income_txn.income_kind = Some(IncomeKind::Budgeted);
        txns.push(income_txn);

        svc.ensure_current_month(user_id, ny_noon(2026, 2, 8))
            .await
            .expect("init feb");

        let feb = months
            .find_by_year_month(user_id, 2026, 2)
            .await
            .unwrap()
            .expect("feb");
        let rollover = txns
            .find_rollover_for_month(feb.id)
            .await
            .unwrap()
            .expect("feb rollover");

        let jan_txns = txns.for_month(jan.id);
        let oracle = oracle_d5_net(&jan_txns, 0); // expected = ZERO

        assert_eq!(
            rollover.amount.as_decimal(),
            oracle,
            "actual_cents={actual_cents}: blank-amount -> net must equal oracle (actual-0=actual)"
        );
        // The rollover must equal the raw actual (no subtraction).
        assert_eq!(
            rollover.amount,
            Money::from_minor(actual_cents),
            "blank-amount: actual flows straight into Other — net = actual"
        );
        assert_eq!(rollover.category_id, Some(rollover_bucket_id));
    }
}

// ===========================================================================
// (c) Surplus routing
// ===========================================================================

/// Build an `IncomeSurplusRouter` wired to fresh fakes.
fn build_router(
    fund_repo: Arc<MemFundRepo>,
    txn_repo: Arc<MemTxnRepo>,
    budget_repo: Arc<MemBudgetRepo>,
) -> IncomeSurplusRouter {
    let uow = Arc::new(NoopUowProvider);
    let fund_svc = Arc::new(FundService::new(fund_repo, txn_repo, budget_repo, uow));
    IncomeSurplusRouter::new(fund_svc)
}

fn surplus_fund(user_id: UserId) -> Fund {
    Fund {
        id: FundId::generate(),
        user_id,
        name: "Surplus target".to_owned(),
        kind: FundKind::Surplus,
        balance: Money::ZERO,
        target_balance: Some(Money::from_major(10_000)),
        compulsory_repayment: false,
        created_at: Utc::now(),
    }
}

/// `this_month`: no fund contribution, no transaction posted. The D5 formula
/// already raises Other; the router is a pure no-op.
#[tokio::test]
async fn this_month_routing_is_noop_no_transaction_posted() {
    let user_id = UserId::generate();
    let fund_repo = Arc::new(MemFundRepo::default());
    let txn_repo = Arc::new(MemTxnRepo::default());
    let budget_repo = Arc::new(MemBudgetRepo::default());
    let router = build_router(
        Arc::clone(&fund_repo),
        Arc::clone(&txn_repo),
        Arc::clone(&budget_repo),
    );

    router
        .route_surplus(
            SurplusRouting::ThisMonth,
            Money::from_major(250),
            None,
            MonthId::generate(),
            CategoryId::generate(),
            ymd(2026, 6, 15),
            Utc::now(),
        )
        .await
        .expect("this_month succeeds");

    let _ = user_id;
    let posted = txn_repo.all();
    assert!(
        posted.is_empty(),
        "this_month must post zero transactions (D5 formula already raised Other)"
    );
}

/// `buffer` routing: surplus becomes a fund contribution — balance rises by
/// exactly the surplus, exactly ONE transaction is posted, it is a counted
/// Other-bucket expense (`is_fund_draw = false`), and the dollar is counted
/// exactly once (`BUDGET-FUND-EARMARK-1`).
#[tokio::test]
async fn buffer_routing_contributes_into_fund_counted_once() {
    let user_id = UserId::generate();
    let fund = surplus_fund(user_id);
    let fund_id = fund.id;
    let earmark_cat = CategoryId::generate();

    let fund_repo = Arc::new(MemFundRepo::with_fund(fund));
    let txn_repo = Arc::new(MemTxnRepo::default());
    let budget_repo = Arc::new(MemBudgetRepo::default());
    let router = build_router(
        Arc::clone(&fund_repo),
        Arc::clone(&txn_repo),
        Arc::clone(&budget_repo),
    );

    let surplus = Money::from_minor(30_000); // $300.00

    router
        .route_surplus(
            SurplusRouting::Buffer,
            surplus,
            Some(fund_id),
            MonthId::generate(),
            earmark_cat,
            ymd(2026, 6, 15),
            Utc::now(),
        )
        .await
        .expect("buffer route succeeds");

    // Fund balance rose by exactly the surplus.
    let stored = fund_repo.find_by_id(fund_id).await.unwrap().expect("fund");
    assert_eq!(
        stored.balance, surplus,
        "fund balance must rise by exactly the surplus"
    );

    // Exactly one transaction posted.
    let posted = txn_repo.all();
    assert_eq!(posted.len(), 1, "exactly one transaction must be posted");

    let t = &posted[0];
    // The contribution is an outflow (negative amount) — a counted Other expense.
    assert_eq!(
        t.amount, -surplus,
        "contribution must post as negative (Other-bucket expense)"
    );
    assert_eq!(
        t.category_id,
        Some(earmark_cat),
        "contribution must use the earmark category"
    );
    assert!(
        !t.is_fund_draw,
        "contribution must be is_fund_draw=false so it COUNTS (BUDGET-FUND-EARMARK-1)"
    );

    // Conservation: Other-net reduction (−$300) + fund-balance gain (+$300) = 0.
    // The dollar is counted exactly once through the Other expense.
    assert_eq!(
        t.amount + stored.balance,
        Money::ZERO,
        "conservation: Other-net reduction + fund gain = 0 (counted once)"
    );
}

/// `savings` routing: same as `buffer` — surplus becomes a fund contribution
/// into the target fund, balance rises, one transaction posted, counted once.
#[tokio::test]
async fn savings_routing_contributes_into_fund_counted_once() {
    let user_id = UserId::generate();
    let fund = surplus_fund(user_id);
    let fund_id = fund.id;

    let fund_repo = Arc::new(MemFundRepo::with_fund(fund));
    let txn_repo = Arc::new(MemTxnRepo::default());
    let budget_repo = Arc::new(MemBudgetRepo::default());
    let router = build_router(
        Arc::clone(&fund_repo),
        Arc::clone(&txn_repo),
        Arc::clone(&budget_repo),
    );

    let surplus = Money::from_minor(17_333); // $173.33 — awkward cents

    router
        .route_surplus(
            SurplusRouting::Savings,
            surplus,
            Some(fund_id),
            MonthId::generate(),
            CategoryId::generate(),
            ymd(2026, 6, 15),
            Utc::now(),
        )
        .await
        .expect("savings route succeeds");

    let stored = fund_repo.find_by_id(fund_id).await.unwrap().expect("fund");
    assert_eq!(
        stored.balance, surplus,
        "savings: fund balance must rise by the surplus"
    );

    let posted = txn_repo.all();
    assert_eq!(posted.len(), 1, "savings: exactly one transaction posted");
    assert_eq!(
        posted[0].amount, -surplus,
        "savings: contribution is a negative (counted expense)"
    );
    assert!(
        !posted[0].is_fund_draw,
        "savings: must be is_fund_draw=false (COUNTS)"
    );
}

/// Per-transaction override wins over the config default, in both directions.
/// `effective_routing` is a pure function — verified without I/O.
#[test]
fn effective_routing_override_wins_both_directions() {
    // Default=Buffer, no override -> Buffer.
    assert_eq!(
        IncomeSurplusRouter::effective_routing(SurplusRouting::Buffer, None),
        SurplusRouting::Buffer,
        "no override -> default is used"
    );

    // Default=Buffer, override=ThisMonth -> ThisMonth.
    assert_eq!(
        IncomeSurplusRouter::effective_routing(
            SurplusRouting::Buffer,
            Some(SurplusRouting::ThisMonth),
        ),
        SurplusRouting::ThisMonth,
        "override ThisMonth wins over default Buffer"
    );

    // Default=ThisMonth, override=Savings -> Savings.
    assert_eq!(
        IncomeSurplusRouter::effective_routing(
            SurplusRouting::ThisMonth,
            Some(SurplusRouting::Savings),
        ),
        SurplusRouting::Savings,
        "override Savings wins over default ThisMonth"
    );

    // Default=Savings, override=Buffer -> Buffer.
    assert_eq!(
        IncomeSurplusRouter::effective_routing(
            SurplusRouting::Savings,
            Some(SurplusRouting::Buffer),
        ),
        SurplusRouting::Buffer,
        "override Buffer wins over default Savings"
    );
}

/// Property: across 1 000 seeded random (default, override) pairs, the
/// effective routing always equals the override when present, else the default.
/// This exercises the full enum cross-product under arbitrary ordering.
#[test]
fn effective_routing_override_wins_property_1000_seeds() {
    let all_routings: [SurplusRouting; 3] = [
        SurplusRouting::Buffer,
        SurplusRouting::ThisMonth,
        SurplusRouting::Savings,
    ];

    let mut rng = SplitMix64::new(0xFEED_FACE_CAFE_BABE);

    for seed_idx in 0..1_000_u64 {
        let default_idx = (rng.next_u64() % 3) as usize;
        let has_override = rng.bool();
        let override_idx = (rng.next_u64() % 3) as usize;

        let default_routing = all_routings[default_idx];
        let override_routing = if has_override {
            Some(all_routings[override_idx])
        } else {
            None
        };

        let effective = IncomeSurplusRouter::effective_routing(default_routing, override_routing);
        let expected = override_routing.unwrap_or(default_routing);

        assert_eq!(
            effective, expected,
            "seed_idx={seed_idx}: effective_routing({default_routing:?}, {override_routing:?}) \
             must equal {expected:?}"
        );
    }
}

/// Non-positive surplus is rejected with a typed `ServiceError::Domain` error
/// before any fund operation is attempted.
#[tokio::test]
async fn zero_surplus_is_rejected_before_fund_op() {
    let user_id = UserId::generate();
    let fund = surplus_fund(user_id);
    let fund_id = fund.id;
    let fund_repo = Arc::new(MemFundRepo::with_fund(fund));
    let txn_repo = Arc::new(MemTxnRepo::default());
    let budget_repo = Arc::new(MemBudgetRepo::default());
    let router = build_router(
        Arc::clone(&fund_repo),
        Arc::clone(&txn_repo),
        Arc::clone(&budget_repo),
    );

    let err = router
        .route_surplus(
            SurplusRouting::Buffer,
            Money::ZERO,
            Some(fund_id),
            MonthId::generate(),
            CategoryId::generate(),
            ymd(2026, 6, 15),
            Utc::now(),
        )
        .await
        .expect_err("zero surplus must be rejected");

    assert!(
        matches!(err, ServiceError::Domain(_)),
        "zero surplus must surface as ServiceError::Domain, got {err:?}"
    );
    // No transaction must have been posted.
    assert!(
        txn_repo.all().is_empty(),
        "zero-surplus rejection must post no transaction"
    );
}

/// Buffer/savings routing without a target fund id returns a `ServiceError::Domain`.
#[tokio::test]
async fn buffer_routing_without_fund_id_is_a_domain_error() {
    let fund_repo = Arc::new(MemFundRepo::default());
    let txn_repo = Arc::new(MemTxnRepo::default());
    let budget_repo = Arc::new(MemBudgetRepo::default());
    let router = build_router(fund_repo, txn_repo, budget_repo);

    for routing in [SurplusRouting::Buffer, SurplusRouting::Savings] {
        let err = router
            .route_surplus(
                routing,
                Money::from_major(100),
                None, // missing fund id
                MonthId::generate(),
                CategoryId::generate(),
                ymd(2026, 6, 15),
                Utc::now(),
            )
            .await
            .expect_err("buffer/savings without fund id must fail");

        assert!(
            matches!(err, ServiceError::Domain(_)),
            "{routing:?} without fund_id must surface as ServiceError::Domain, got {err:?}"
        );
    }
}

/// Property: across 200 seeded surplus amounts, buffer and savings routing each
/// increment the fund balance by exactly the surplus and post exactly one
/// counted Other-bucket expense (dollar counted once, `BUDGET-FUND-EARMARK-1`).
#[tokio::test]
async fn surplus_routing_fund_contribution_property_counted_once() {
    let mut rng = SplitMix64::new(0x0102_0304_0506_0708);

    for seed_idx in 0..200_u64 {
        let surplus_cents = rng.cents(1, 500_000); // $0.01 to $5,000.00
        let use_savings = rng.bool(); // Buffer or Savings

        let user_id = UserId::generate();
        let fund = Fund {
            id: FundId::generate(),
            user_id,
            name: "prop fund".to_owned(),
            // Both Buffer and Savings routing use the same contribute() plumbing;
            // the fund kind is Surplus for both arms (no compulsory repayment).
            kind: FundKind::Surplus,
            balance: Money::ZERO,
            target_balance: None,
            compulsory_repayment: false,
            created_at: Utc::now(),
        };
        let fund_id = fund.id;

        let fund_repo = Arc::new(MemFundRepo::with_fund(fund));
        let txn_repo = Arc::new(MemTxnRepo::default());
        let budget_repo = Arc::new(MemBudgetRepo::default());
        let router = build_router(
            Arc::clone(&fund_repo),
            Arc::clone(&txn_repo),
            Arc::clone(&budget_repo),
        );

        let routing = if use_savings {
            SurplusRouting::Savings
        } else {
            SurplusRouting::Buffer
        };
        let surplus = Money::from_minor(surplus_cents);

        router
            .route_surplus(
                routing,
                surplus,
                Some(fund_id),
                MonthId::generate(),
                CategoryId::generate(),
                ymd(2026, 6, 15),
                Utc::now(),
            )
            .await
            .expect("routing succeeds");

        let stored = fund_repo.find_by_id(fund_id).await.unwrap().expect("fund");
        assert_eq!(
            stored.balance, surplus,
            "seed_idx={seed_idx} routing={routing:?}: fund balance must equal surplus"
        );

        let posted = txn_repo.all();
        assert_eq!(
            posted.len(),
            1,
            "seed_idx={seed_idx}: exactly one transaction posted"
        );
        assert_eq!(
            posted[0].amount, -surplus,
            "seed_idx={seed_idx}: contribution must be negative (expense)"
        );
        assert!(
            !posted[0].is_fund_draw,
            "seed_idx={seed_idx}: must be is_fund_draw=false (COUNTS in net)"
        );

        // Conservation: Other-expense + fund-gain = 0.
        assert_eq!(
            posted[0].amount + stored.balance,
            Money::ZERO,
            "seed_idx={seed_idx}: conservation invariant broken"
        );
    }
}

// ===========================================================================
// (d) Stubbed modes fail loudly-but-safely — no panic, no silent wrong number
// ===========================================================================

/// Every unbuilt arm of the mode × cadence matrix returns
/// `ServiceError::UnsupportedIncome` and NEVER panics, for a representative
/// set of non-zero amounts.
#[test]
fn stubbed_paths_return_unsupported_income_not_panic() {
    // The 6 unbuilt combinations (SPEC §4.8, "design-complete, build-what-you-use"):
    //   - Smoothed × Semimonthly, Biweekly, Weekly
    //   - PerPaycheck × Biweekly, Weekly
    //   - PerPaycheck × Hourly (contradictory: fixed amount on hourly)
    let stubbed_cases: &[(IncomeMode, PaycheckType)] = &[
        (IncomeMode::Smoothed, PaycheckType::Semimonthly),
        (IncomeMode::Smoothed, PaycheckType::Biweekly),
        (IncomeMode::Smoothed, PaycheckType::Weekly),
        (IncomeMode::PerPaycheck, PaycheckType::Biweekly),
        (IncomeMode::PerPaycheck, PaycheckType::Weekly),
        (IncomeMode::PerPaycheck, PaycheckType::Hourly),
    ];

    // Representative amounts — the error must not be amount-dependent.
    let amounts: &[i64] = &[1, 100_000, 245_099, 999_999, 1_000_000];

    for &(mode, cadence) in stubbed_cases {
        for &cents in amounts {
            let user_id = UserId::generate();
            let cfg = config(user_id, mode, cadence, Some(Money::from_minor(cents)));

            let result = ConfigDrivenIncomeExpectation::from_config(&cfg);
            assert!(
                result.is_err(),
                "{mode:?}/{cadence:?} cents={cents}: expected Err, got Ok"
            );
            let err = result.unwrap_err();
            assert!(
                matches!(err, ServiceError::UnsupportedIncome { .. }),
                "{mode:?}/{cadence:?} cents={cents}: must be UnsupportedIncome, got {err:?}"
            );

            // The error must carry the correct mode and cadence fields.
            if let ServiceError::UnsupportedIncome {
                mode: err_mode,
                cadence: err_cadence,
                ..
            } = err
            {
                assert_eq!(
                    err_mode, mode,
                    "{mode:?}/{cadence:?}: error mode field mismatch"
                );
                assert_eq!(
                    err_cadence, cadence,
                    "{mode:?}/{cadence:?}: error cadence field mismatch"
                );
            }
        }
    }
}

/// Property: for 300 random amounts, EVERY stubbed arm consistently returns
/// `UnsupportedIncome` — confirming the error is amount-independent and stable.
#[test]
fn stubbed_paths_unsupported_error_is_amount_independent_property() {
    let mut rng = SplitMix64::new(0xDEAD_BEEF_CAFE_BABE);

    let stubbed: &[(IncomeMode, PaycheckType)] = &[
        (IncomeMode::Smoothed, PaycheckType::Semimonthly),
        (IncomeMode::Smoothed, PaycheckType::Biweekly),
        (IncomeMode::Smoothed, PaycheckType::Weekly),
        (IncomeMode::PerPaycheck, PaycheckType::Biweekly),
        (IncomeMode::PerPaycheck, PaycheckType::Weekly),
        (IncomeMode::PerPaycheck, PaycheckType::Hourly),
    ];

    for seed_idx in 0..300_u64 {
        let cents = rng.cents(1, 5_000_000); // $0.01 to $50,000.00
        let arm_idx = (rng.next_u64() % stubbed.len() as u64) as usize;
        let (mode, cadence) = stubbed[arm_idx];
        let user_id = UserId::generate();
        let cfg = config(user_id, mode, cadence, Some(Money::from_minor(cents)));

        let err =
            ConfigDrivenIncomeExpectation::from_config(&cfg).expect_err("stubbed arm must fail");

        assert!(
            matches!(err, ServiceError::UnsupportedIncome { .. }),
            "seed_idx={seed_idx} {mode:?}/{cadence:?} cents={cents}: \
             expected UnsupportedIncome, got {err:?}"
        );
    }
}

/// Stubbed paths via the async `load()` boundary also surface as
/// `UnsupportedIncome` — confirming the error originates in `resolve_per_month`
/// and not in some `load`-specific short-circuit.
#[tokio::test]
async fn stubbed_paths_via_load_also_surface_unsupported_income() {
    let stubbed: &[(IncomeMode, PaycheckType)] = &[
        (IncomeMode::Smoothed, PaycheckType::Biweekly),
        (IncomeMode::PerPaycheck, PaycheckType::Weekly),
        (IncomeMode::PerPaycheck, PaycheckType::Hourly),
    ];

    for &(mode, cadence) in stubbed {
        let user_id = UserId::generate();
        let cfg = config(user_id, mode, cadence, Some(Money::from_minor(200_000)));
        let repo = MemPaycheckConfigRepo::with(cfg);
        let err = ConfigDrivenIncomeExpectation::load(&repo, user_id)
            .await
            .expect_err("stubbed via load() must fail");
        assert!(
            matches!(err, ServiceError::UnsupportedIncome { .. }),
            "load() path: {mode:?}/{cadence:?} must surface UnsupportedIncome, got {err:?}"
        );
    }
}

/// `Smoothed` cadence covers are NOT silently swallowed — each cadence in the
/// smoothed arm produces `UnsupportedIncome { mode: Smoothed, .. }` with the
/// correct cadence threaded through.
#[test]
fn smoothed_all_cadences_carry_correct_mode_and_cadence_in_error() {
    for cadence in [
        PaycheckType::Semimonthly,
        PaycheckType::Biweekly,
        PaycheckType::Weekly,
    ] {
        let user_id = UserId::generate();
        let cfg = config(
            user_id,
            IncomeMode::Smoothed,
            cadence,
            Some(Money::from_major(2_000)),
        );
        let err = ConfigDrivenIncomeExpectation::from_config(&cfg).expect_err("smoothed must fail");
        match err {
            ServiceError::UnsupportedIncome {
                mode: IncomeMode::Smoothed,
                cadence: err_cadence,
                detail,
            } => {
                assert_eq!(
                    err_cadence, cadence,
                    "smoothed/{cadence:?}: error cadence field must match the config cadence"
                );
                assert!(
                    !detail.is_empty(),
                    "smoothed/{cadence:?}: detail must be non-empty"
                );
            }
            other => panic!(
                "smoothed/{cadence:?}: expected UnsupportedIncome{{mode=Smoothed}}, got {other:?}"
            ),
        }
    }
}
