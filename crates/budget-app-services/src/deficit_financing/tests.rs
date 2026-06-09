//! Adversarial tests for deficit financing (`SPEC §12` D9,
//! `BUDGET-DEFICIT-FINANCING-1`).
//!
//! Break-don't-confirm: every assertion is checked against an INDEPENDENT
//! `rust_decimal` oracle (the deficit re-derived without the build's netting, the
//! installment sum re-derived without the build's clamp), never the build's own
//! arithmetic. The properties proven:
//!   - **threshold boundary** — a deficit just UNDER 75% of next month's Other →
//!     `None`; just OVER → `Some` (strict `>`, exactly-at rolls forward);
//!   - **amortization exactness** — the installments sum EXACTLY to the financed
//!     principal, no rounding leak (last installment absorbs the remainder);
//!   - **installment-1 absorption** — accepting financing makes next month's Other
//!     absorb ONLY installment 1, not the full deficit;
//!   - **default path** — below threshold OR declined, the full deficit rolls
//!     forward unchanged (the lifecycle rollover is the whole deficit);
//!   - **count-once** — the financed deficit is counted exactly once across the
//!     whole chain (suppressed rollover + the N installments == the deficit).
//!
//! ### Lint suppressions (test-only)
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]
#![allow(clippy::panic)]
#![allow(clippy::too_many_lines)]

use std::any::Any;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::{NaiveDate, Utc};
use rust_decimal::Decimal;

use budget_domain::budget::Budget;
use budget_domain::category::Category;
use budget_domain::enums::{
    Cadence, CategoryGrp, FundKind, MonthStatus, ObligationSource, ObligationStatus,
    TransactionSource, TransactionStatus,
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

use crate::config::DeficitFinancingConfig;
use crate::deficit_financing::DeficitFinancingService;
use crate::fund::FundService;
use crate::income::IncomeExpectation;
use crate::month_lifecycle::MonthLifecycleService;

use super::DeficitFinancingOffer;

// ---------------------------------------------------------------------------
// UoW fakes (mirror fund::tests)
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

// ---------------------------------------------------------------------------
// Repository fakes
// ---------------------------------------------------------------------------

#[derive(Default)]
struct FundStore {
    funds: Vec<Fund>,
    obligations: Vec<RepaymentObligation>,
}

struct FakeFundRepo {
    store: Mutex<FundStore>,
}

#[async_trait]
impl FundRepository for FakeFundRepo {
    async fn find_by_id(&self, id: FundId) -> Result<Option<Fund>, RepositoryError> {
        Ok(self
            .store
            .lock()
            .map_err(poisoned)?
            .funds
            .iter()
            .find(|f| f.id == id)
            .cloned())
    }
    async fn list_for_user(&self, user_id: UserId) -> Result<Vec<Fund>, RepositoryError> {
        Ok(self
            .store
            .lock()
            .map_err(poisoned)?
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
        let mut s = self.store.lock().map_err(poisoned)?;
        if let Some(slot) = s.funds.iter_mut().find(|f| f.id == fund.id) {
            *slot = fund.clone();
        } else {
            s.funds.push(fund.clone());
        }
        Ok(())
    }
    async fn find_obligation(
        &self,
        id: RepaymentObligationId,
    ) -> Result<Option<RepaymentObligation>, RepositoryError> {
        Ok(self
            .store
            .lock()
            .map_err(poisoned)?
            .obligations
            .iter()
            .find(|o| o.id == id)
            .cloned())
    }
    async fn list_active_obligations(
        &self,
        user_id: UserId,
    ) -> Result<Vec<RepaymentObligation>, RepositoryError> {
        Ok(self
            .store
            .lock()
            .map_err(poisoned)?
            .obligations
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
            .store
            .lock()
            .map_err(poisoned)?
            .obligations
            .iter()
            .find(|o| o.transaction_id == Some(transaction_id))
            .cloned())
    }
    async fn find_active_deficit_obligation_for_month(
        &self,
        month_id: MonthId,
    ) -> Result<Option<RepaymentObligation>, RepositoryError> {
        Ok(self
            .store
            .lock()
            .map_err(poisoned)?
            .obligations
            .iter()
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
        Ok(self
            .store
            .lock()
            .map_err(poisoned)?
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
        let mut s = self.store.lock().map_err(poisoned)?;
        if let Some(slot) = s.obligations.iter_mut().find(|o| o.id == obligation.id) {
            *slot = obligation.clone();
        } else {
            s.obligations.push(obligation.clone());
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

#[async_trait]
impl BudgetRepository for FakeBudgetRepo {
    async fn find_by_id(&self, id: BudgetId) -> Result<Option<Budget>, RepositoryError> {
        Ok(self
            .store
            .lock()
            .map_err(poisoned)?
            .budgets
            .iter()
            .find(|b| b.id == id)
            .cloned())
    }
    async fn find_active_for_date(
        &self,
        user_id: UserId,
        _date: NaiveDate,
    ) -> Result<Option<Budget>, RepositoryError> {
        // Single budget version in these tests: active for any date.
        Ok(self
            .store
            .lock()
            .map_err(poisoned)?
            .budgets
            .iter()
            .find(|b| b.user_id == user_id)
            .cloned())
    }
    async fn find_current(&self, user_id: UserId) -> Result<Option<Budget>, RepositoryError> {
        Ok(self
            .store
            .lock()
            .map_err(poisoned)?
            .budgets
            .iter()
            .find(|b| b.user_id == user_id)
            .cloned())
    }
    async fn list_for_user(&self, _user_id: UserId) -> Result<Vec<Budget>, RepositoryError> {
        Ok(Vec::new())
    }
    async fn list_categories(&self, budget_id: BudgetId) -> Result<Vec<Category>, RepositoryError> {
        Ok(self
            .store
            .lock()
            .map_err(poisoned)?
            .categories
            .iter()
            .filter(|c| c.budget_id == budget_id)
            .cloned()
            .collect())
    }
    async fn find_category(&self, id: CategoryId) -> Result<Option<Category>, RepositoryError> {
        Ok(self
            .store
            .lock()
            .map_err(poisoned)?
            .categories
            .iter()
            .find(|c| c.id == id)
            .cloned())
    }
    async fn find_rollover_bucket(
        &self,
        budget_id: BudgetId,
    ) -> Result<Option<Category>, RepositoryError> {
        Ok(self
            .store
            .lock()
            .map_err(poisoned)?
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
        let mut s = self.store.lock().map_err(poisoned)?;
        if let Some(slot) = s.budgets.iter_mut().find(|b| b.id == budget.id) {
            *slot = budget.clone();
        } else {
            s.budgets.push(budget.clone());
        }
        Ok(())
    }
    async fn save_category(
        &self,
        category: &Category,
        _uow: Option<&dyn UnitOfWork>,
    ) -> Result<(), RepositoryError> {
        let mut s = self.store.lock().map_err(poisoned)?;
        if let Some(slot) = s.categories.iter_mut().find(|c| c.id == category.id) {
            *slot = category.clone();
        } else {
            s.categories.push(category.clone());
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
    fn for_month(&self, month_id: MonthId) -> Vec<Transaction> {
        self.store
            .lock()
            .unwrap()
            .txns
            .iter()
            .filter(|t| t.month_id == month_id)
            .cloned()
            .collect()
    }
}

#[async_trait]
impl TransactionRepository for FakeTransactionRepo {
    async fn find_by_id(&self, id: TransactionId) -> Result<Option<Transaction>, RepositoryError> {
        Ok(self
            .store
            .lock()
            .map_err(poisoned)?
            .txns
            .iter()
            .find(|t| t.id == id)
            .cloned())
    }
    async fn list_for_month(&self, month_id: MonthId) -> Result<Vec<Transaction>, RepositoryError> {
        Ok(self.for_month(month_id))
    }
    async fn list_for_category_in_month(
        &self,
        month_id: MonthId,
        category_id: CategoryId,
    ) -> Result<Vec<Transaction>, RepositoryError> {
        Ok(self
            .store
            .lock()
            .map_err(poisoned)?
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
        Ok(self
            .store
            .lock()
            .map_err(poisoned)?
            .txns
            .iter()
            .find(|t| t.month_id == month_id && t.is_rollover)
            .cloned())
    }
    async fn find_by_plaid_transaction_id(
        &self,
        _plaid_transaction_id: &str,
    ) -> Result<Option<Transaction>, RepositoryError> {
        Ok(None)
    }
    async fn list_expected_for_month(
        &self,
        _month_id: MonthId,
    ) -> Result<Vec<Transaction>, RepositoryError> {
        Ok(Vec::new())
    }
    async fn find_expected_matched_to(
        &self,
        _real_transaction_id: TransactionId,
    ) -> Result<Option<Transaction>, RepositoryError> {
        Ok(None)
    }
    async fn category_spent_for_month(
        &self,
        _month_id: MonthId,
    ) -> Result<Vec<CategorySpent>, RepositoryError> {
        Ok(Vec::new())
    }
    async fn month_net(&self, month_id: MonthId) -> Result<MonthNet, RepositoryError> {
        Ok(MonthNet {
            month_id,
            net: Money::ZERO,
        })
    }
    async fn save(
        &self,
        transaction: &Transaction,
        _uow: Option<&dyn UnitOfWork>,
    ) -> Result<(), RepositoryError> {
        let mut s = self.store.lock().map_err(poisoned)?;
        if let Some(slot) = s.txns.iter_mut().find(|t| t.id == transaction.id) {
            *slot = transaction.clone();
        } else {
            s.txns.push(transaction.clone());
        }
        Ok(())
    }
    async fn delete(
        &self,
        id: TransactionId,
        _uow: Option<&dyn UnitOfWork>,
    ) -> Result<(), RepositoryError> {
        self.store
            .lock()
            .map_err(poisoned)?
            .txns
            .retain(|t| t.id != id);
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

#[async_trait]
impl MonthRepository for FakeMonthRepo {
    async fn find_by_id(&self, id: MonthId) -> Result<Option<Month>, RepositoryError> {
        Ok(self
            .store
            .lock()
            .map_err(poisoned)?
            .months
            .iter()
            .find(|m| m.id == id)
            .cloned())
    }
    async fn find_by_year_month(
        &self,
        user_id: UserId,
        year: i32,
        month: i32,
    ) -> Result<Option<Month>, RepositoryError> {
        Ok(self
            .store
            .lock()
            .map_err(poisoned)?
            .months
            .iter()
            .find(|m| m.user_id == user_id && m.year == year && m.month == month)
            .cloned())
    }
    async fn find_latest(&self, user_id: UserId) -> Result<Option<Month>, RepositoryError> {
        Ok(self
            .store
            .lock()
            .map_err(poisoned)?
            .months
            .iter()
            .filter(|m| m.user_id == user_id)
            .max_by_key(|m| (m.year, m.month))
            .cloned())
    }
    async fn list_for_user(&self, user_id: UserId) -> Result<Vec<Month>, RepositoryError> {
        Ok(self
            .store
            .lock()
            .map_err(poisoned)?
            .months
            .iter()
            .filter(|m| m.user_id == user_id)
            .cloned()
            .collect())
    }
    async fn create_if_absent(
        &self,
        month: &Month,
        _uow: Option<&dyn UnitOfWork>,
    ) -> Result<Month, RepositoryError> {
        let mut s = self.store.lock().map_err(poisoned)?;
        if let Some(existing) = s
            .months
            .iter()
            .find(|m| m.user_id == month.user_id && m.year == month.year && m.month == month.month)
        {
            return Ok(existing.clone());
        }
        s.months.push(month.clone());
        Ok(month.clone())
    }
    async fn save(
        &self,
        month: &Month,
        _uow: Option<&dyn UnitOfWork>,
    ) -> Result<(), RepositoryError> {
        let mut s = self.store.lock().map_err(poisoned)?;
        if let Some(slot) = s.months.iter_mut().find(|m| m.id == month.id) {
            *slot = month.clone();
        } else {
            s.months.push(month.clone());
        }
        Ok(())
    }
}

/// A fixed expected-income stub: returns a constant figure for every month.
struct FixedIncome(Money);
impl IncomeExpectation for FixedIncome {
    fn expected_income(&self, _user: UserId, _year: i32, _month: i32) -> Money {
        self.0
    }
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

struct Harness {
    deficit: DeficitFinancingService,
    lifecycle: Arc<MonthLifecycleService>,
    funds: Arc<FakeFundRepo>,
    transactions: Arc<FakeTransactionRepo>,
    user_id: UserId,
    fund_id: FundId,
    closed_month: Month,
    next_month: Month,
    /// The next month's rollover ("Other") bucket.
    rollover_bucket_id: CategoryId,
}

fn ymd(y: i32, m: u32, d: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, d).unwrap_or(NaiveDate::MIN)
}

/// Build a harness whose closed month nets to `-deficit` (a settled expense of
/// `deficit` against a normal category, expected income matched to actual so the
/// income term is zero), and whose NEXT month's Other budget is
/// `next_other_budget`. `threshold` pins the financing threshold ratio.
fn harness(deficit: Money, next_other_budget: Money, threshold: Decimal) -> Harness {
    let funds = Arc::new(FakeFundRepo {
        store: Mutex::new(FundStore::default()),
    });
    let transactions = Arc::new(FakeTransactionRepo {
        store: Mutex::new(TxnStore::default()),
    });
    let budgets = Arc::new(FakeBudgetRepo {
        store: Mutex::new(BudgetStore::default()),
    });
    let months = Arc::new(FakeMonthRepo {
        store: Mutex::new(MonthStore::default()),
    });

    let user_id = UserId::generate();
    let budget_id = BudgetId::generate();
    let closed_id = MonthId::generate();
    let next_id = MonthId::generate();
    let spend_category_id = CategoryId::generate();
    let rollover_bucket_id = CategoryId::generate();
    let fund_id = FundId::generate();

    let closed_month = Month {
        id: closed_id,
        user_id,
        budget_id,
        year: 2026,
        month: 7,
        status: MonthStatus::Closed,
        opened_at: Utc::now(),
        closed_at: Some(Utc::now()),
    };
    let next_month = Month {
        id: next_id,
        user_id,
        budget_id,
        year: 2026,
        month: 8,
        status: MonthStatus::Open,
        opened_at: Utc::now(),
        closed_at: None,
    };

    {
        let mut s = budgets.store.lock().unwrap();
        s.budgets.push(Budget {
            id: budget_id,
            user_id,
            name: "v1".to_owned(),
            effective_from: ymd(2026, 1, 1),
            effective_to: None,
            created_at: Utc::now(),
        });
        // The rollover ("Other") bucket — its `amount` is next month's Other budget.
        s.categories.push(category(
            rollover_bucket_id,
            budget_id,
            next_other_budget,
            true,
        ));
        // A normal spending category carrying the deficit.
        s.categories
            .push(category(spend_category_id, budget_id, Money::ZERO, false));
    }
    {
        let mut s = months.store.lock().unwrap();
        s.months.push(closed_month.clone());
        s.months.push(next_month.clone());
    }
    // The closed month's deficit: one settled expense of `deficit` against the
    // normal category. Expected income == 0 so the income term contributes nothing;
    // the net is exactly -deficit.
    {
        let mut s = transactions.store.lock().unwrap();
        s.txns.push(expense(
            user_id,
            closed_id,
            spend_category_id,
            deficit,
            ymd(2026, 7, 15),
        ));
    }
    // The buffer that anchors the obligation.
    {
        let mut s = funds.store.lock().unwrap();
        s.funds.push(Fund {
            id: fund_id,
            user_id,
            name: "Buffer".to_owned(),
            kind: FundKind::Buffer,
            balance: Money::ZERO,
            target_balance: None,
            compulsory_repayment: true,
            created_at: Utc::now(),
        });
    }

    let lifecycle = Arc::new(MonthLifecycleService::new(
        Arc::clone(&months) as Arc<dyn MonthRepository>,
        Arc::clone(&budgets) as Arc<dyn BudgetRepository>,
        Arc::clone(&transactions) as Arc<dyn TransactionRepository>,
        Arc::clone(&funds) as Arc<dyn FundRepository>,
        Arc::new(FakeUowProvider) as Arc<dyn UowProvider>,
        Arc::new(FixedIncome(Money::ZERO)) as Arc<dyn IncomeExpectation>,
    ));

    let deficit_svc = DeficitFinancingService::new(
        Arc::clone(&lifecycle),
        Arc::clone(&budgets) as Arc<dyn BudgetRepository>,
        Arc::clone(&transactions) as Arc<dyn TransactionRepository>,
        Arc::clone(&funds) as Arc<dyn FundRepository>,
        Arc::new(FakeUowProvider) as Arc<dyn UowProvider>,
        DeficitFinancingConfig::with_threshold(threshold),
    );

    Harness {
        deficit: deficit_svc,
        lifecycle,
        funds,
        transactions,
        user_id,
        fund_id,
        closed_month,
        next_month,
        rollover_bucket_id,
    }
}

fn category(
    id: CategoryId,
    budget_id: BudgetId,
    amount: Money,
    is_rollover_bucket: bool,
) -> Category {
    Category {
        id,
        budget_id,
        category_key: CategoryKey::generate(),
        name: if is_rollover_bucket { "Other" } else { "Spend" }.to_owned(),
        amount,
        grp: CategoryGrp::Discretionary,
        settle_type: None,
        expected_bills: None,
        is_rollover_bucket,
        cadence: Cadence::Monthly,
        period_months: None,
        fund_balance: Money::ZERO,
        next_due_date: None,
        sort_order: 1,
    }
}

fn expense(
    user_id: UserId,
    month_id: MonthId,
    category_id: CategoryId,
    magnitude: Money,
    date: NaiveDate,
) -> Transaction {
    Transaction {
        id: TransactionId::generate(),
        user_id,
        month_id,
        category_id: Some(category_id),
        account_id: None,
        date,
        amount: -magnitude,
        description: "spend".to_owned(),
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

/// Independent oracle: the net (settled) outflow of the closed month, summed from
/// raw `Decimal` amounts — re-derives the deficit WITHOUT the build's
/// `month_net_for`. Income term is zero in these harnesses.
fn oracle_closed_month_net(h: &Harness) -> Decimal {
    h.transactions
        .for_month(h.closed_month.id)
        .iter()
        .map(|t| t.amount.as_decimal())
        .sum()
}

/// Independent oracle: the sum of every NON-rollover expense booked into the next
/// month's Other bucket (the deficit-financing installments), as raw `Decimal`.
fn oracle_next_month_other_installments(h: &Harness) -> Decimal {
    h.transactions
        .for_month(h.next_month.id)
        .iter()
        .filter(|t| t.category_id == Some(h.rollover_bucket_id) && !t.is_rollover)
        .map(|t| -t.amount.as_decimal())
        .sum()
}

// ===========================================================================
// 1. Threshold boundary (just-under -> None, just-over -> Some)
// ===========================================================================

#[tokio::test]
async fn deficit_just_under_threshold_offers_nothing() {
    // Next month Other = $1000; threshold 75% -> $750. A $749.99 deficit is UNDER.
    let h = harness(
        Money::from_decimal(Decimal::new(74_999, 2)),
        Money::from_major(1_000),
        Decimal::new(75, 2),
    );
    // Oracle: deficit magnitude < 0.75 * 1000.
    assert!(oracle_closed_month_net(&h).abs() < Decimal::new(75, 2) * Decimal::from(1_000));
    let offer = h
        .deficit
        .detect_financeable_deficit(&h.closed_month)
        .await
        .expect("detect");
    assert!(offer.is_none(), "sub-threshold deficit must NOT be offered");
}

#[tokio::test]
async fn deficit_exactly_at_threshold_offers_nothing() {
    // Exactly 75% of $1000 = $750. Strict `>` so AT-threshold rolls forward.
    let h = harness(
        Money::from_major(750),
        Money::from_major(1_000),
        Decimal::new(75, 2),
    );
    let offer = h
        .deficit
        .detect_financeable_deficit(&h.closed_month)
        .await
        .expect("detect");
    assert!(
        offer.is_none(),
        "a deficit exactly AT the threshold rolls forward (strict >)"
    );
}

#[tokio::test]
async fn deficit_just_over_threshold_is_offered() {
    // $750.01 > 0.75 * $1000.
    let h = harness(
        Money::from_decimal(Decimal::new(75_001, 2)),
        Money::from_major(1_000),
        Decimal::new(75, 2),
    );
    let offer: DeficitFinancingOffer = h
        .deficit
        .detect_financeable_deficit(&h.closed_month)
        .await
        .expect("detect")
        .expect("over threshold must offer");
    assert_eq!(
        offer.deficit_amount,
        Money::from_decimal(Decimal::new(75_001, 2))
    );
    assert_eq!(offer.next_month_other_budget, Money::from_major(1_000));
    assert_eq!(offer.threshold, Decimal::new(75, 2));
    // Oracle: deficit magnitude strictly exceeds 0.75 * 1000.
    assert!(oracle_closed_month_net(&h).abs() > Decimal::new(75, 2) * Decimal::from(1_000));
}

#[tokio::test]
async fn a_surplus_month_is_never_offered() {
    // No deficit at all: build a harness then assert a surplus closed month yields
    // None (net >= 0). We model "surplus" as zero spend (net == 0).
    let h = harness(Money::ZERO, Money::from_major(1_000), Decimal::new(75, 2));
    let offer = h
        .deficit
        .detect_financeable_deficit(&h.closed_month)
        .await
        .expect("detect");
    assert!(offer.is_none(), "a non-deficit month is never financeable");
}

// ===========================================================================
// 2. Amortization installments sum EXACTLY to the principal (no rounding leak)
// ===========================================================================

#[tokio::test]
async fn installments_sum_exactly_to_principal_over_full_repayment() {
    // $1000 deficit / 3 months = $333.33 x 2 + $333.34 (last absorbs the remainder).
    let principal = Money::from_major(1_000);
    let h = harness(principal, Money::from_major(1_000), Decimal::new(75, 2));

    // Finance over 3 months. Installment 1 posts into next month now.
    let obligation = h
        .deficit
        .finance_deficit(
            &h.closed_month,
            &h.next_month,
            h.fund_id,
            3,
            ymd(2026, 8, 1),
            Utc::now(),
        )
        .await
        .expect("finance");

    // Drive installments 2 and 3 via the EXISTING fund machinery (post_installment).
    let fund_service = FundService::new(
        Arc::clone(&h.funds) as Arc<dyn FundRepository>,
        Arc::clone(&h.transactions) as Arc<dyn TransactionRepository>,
        // budgets unused by post_installment; reuse a fresh fake.
        Arc::new(FakeBudgetRepo {
            store: Mutex::new(BudgetStore::default()),
        }) as Arc<dyn BudgetRepository>,
        Arc::new(FakeUowProvider) as Arc<dyn UowProvider>,
    );
    let other = h.rollover_bucket_id;
    fund_service
        .post_installment(
            obligation.id,
            h.next_month.id,
            other,
            ymd(2026, 9, 1),
            Utc::now(),
        )
        .await
        .expect("installment 2");
    let final_ob = fund_service
        .post_installment(
            obligation.id,
            h.next_month.id,
            other,
            ymd(2026, 10, 1),
            Utc::now(),
        )
        .await
        .expect("installment 3");

    // Oracle: the three Other installments sum to EXACTLY $1000.00, no cent lost.
    let total_installments = oracle_next_month_other_installments(&h);
    assert_eq!(
        total_installments,
        principal.as_decimal(),
        "installments must sum exactly to the principal (no rounding leak)"
    );
    // The obligation is fully repaid.
    assert_eq!(final_ob.remaining_amount, Money::ZERO);
    assert_eq!(final_ob.status, ObligationStatus::Paid);
}

// ===========================================================================
// 3. Accepting financing -> next month's Other absorbs ONLY installment 1
// ===========================================================================

#[tokio::test]
async fn financing_makes_next_month_absorb_only_installment_one() {
    // $900 deficit / 3 months -> installment = $300. Next month's Other should be
    // reduced by ONLY $300 (installment 1), NOT the full $900.
    let h = harness(
        Money::from_major(900),
        Money::from_major(1_000),
        Decimal::new(75, 2),
    );
    let ob = h
        .deficit
        .finance_deficit(
            &h.closed_month,
            &h.next_month,
            h.fund_id,
            3,
            ymd(2026, 8, 1),
            Utc::now(),
        )
        .await
        .expect("finance");

    // Oracle: exactly ONE installment posted into next month's Other = $300.
    let posted = oracle_next_month_other_installments(&h);
    assert_eq!(
        posted,
        Decimal::from(300),
        "next month absorbs only installment 1 ($300), not the full $900 deficit"
    );
    // The obligation reflects installment 1 already paid: remaining = $600.
    assert_eq!(ob.remaining_amount, Money::from_major(600));
    assert_eq!(ob.months_remaining, 2);
    assert_eq!(ob.status, ObligationStatus::Active);
    assert_eq!(ob.source, ObligationSource::Deficit);
    assert_eq!(ob.transaction_id, None);
    assert_eq!(ob.origin_month_id, Some(h.closed_month.id));
}

// ===========================================================================
// 4. Default path: declined / sub-threshold -> full deficit rolls forward
// ===========================================================================

#[tokio::test]
async fn declined_financing_rolls_the_full_deficit_forward() {
    // A financeable $900 deficit that is NOT financed: the lifecycle rollover INTO
    // the next month must be the FULL -$900 (unchanged default, BUDGET-ROLLOVER-INTEGRITY-1).
    let h = harness(
        Money::from_major(900),
        Money::from_major(1_000),
        Decimal::new(75, 2),
    );
    // No finance_deficit call (declined). Compute what would roll forward.
    let rolled = h
        .lifecycle
        .month_net_for(&h.closed_month)
        .await
        .expect("net");
    assert_eq!(
        rolled,
        Money::from_major(-900),
        "declined: the full deficit rolls forward unchanged"
    );
    // And no installment was posted into next month's Other.
    assert_eq!(oracle_next_month_other_installments(&h), Decimal::ZERO);
}

// ===========================================================================
// 5. Count-once: suppressed rollover + installments == the deficit, exactly once
// ===========================================================================

#[tokio::test]
async fn financed_deficit_is_counted_exactly_once_across_the_chain() {
    // $1000 deficit financed over 4 months. After financing:
    //   - the lifecycle rollover INTO next month is SUPPRESSED (zero), and
    //   - installment 1 ($250) is the only Other charge in next month so far.
    // Across the full chain (4 installments) the Other charges sum to EXACTLY the
    // deficit, and the suppressed rollover contributes nothing — so the deficit is
    // counted once (via the installments), never twice.
    let principal = Money::from_major(1_000);
    let h = harness(principal, Money::from_major(1_000), Decimal::new(75, 2));
    let ob = h
        .deficit
        .finance_deficit(
            &h.closed_month,
            &h.next_month,
            h.fund_id,
            4,
            ymd(2026, 8, 1),
            Utc::now(),
        )
        .await
        .expect("finance");

    // Rollover suppression: the lifecycle now rolls ZERO forward out of the closed
    // month (the obligation carries the deficit instead).
    let rolled_after_financing = h
        .lifecycle
        .prior_month_net(h.next_month.user_id, h.next_month.year, h.next_month.month)
        .await
        .expect("prior net");
    assert_eq!(
        rolled_after_financing,
        Money::ZERO,
        "after financing, the financed deficit must NOT also roll forward (no double-count)"
    );

    // Drive installments 2..4.
    let fund_service = FundService::new(
        Arc::clone(&h.funds) as Arc<dyn FundRepository>,
        Arc::clone(&h.transactions) as Arc<dyn TransactionRepository>,
        Arc::new(FakeBudgetRepo {
            store: Mutex::new(BudgetStore::default()),
        }) as Arc<dyn BudgetRepository>,
        Arc::new(FakeUowProvider) as Arc<dyn UowProvider>,
    );
    let other = h.rollover_bucket_id;
    fund_service
        .post_installment(ob.id, h.next_month.id, other, ymd(2026, 9, 1), Utc::now())
        .await
        .expect("inst 2");
    fund_service
        .post_installment(ob.id, h.next_month.id, other, ymd(2026, 10, 1), Utc::now())
        .await
        .expect("inst 3");
    let last = fund_service
        .post_installment(ob.id, h.next_month.id, other, ymd(2026, 11, 1), Utc::now())
        .await
        .expect("inst 4");

    // Count-once oracle: the installments sum to EXACTLY the deficit, the rollover
    // is suppressed (0). deficit counted once = installments; never the deficit again.
    let installments_total = oracle_next_month_other_installments(&h);
    assert_eq!(
        installments_total,
        principal.as_decimal(),
        "the deficit is counted exactly once: installments sum to the principal"
    );
    assert_eq!(rolled_after_financing.as_decimal(), Decimal::ZERO);
    assert_eq!(last.status, ObligationStatus::Paid);
    assert_eq!(last.remaining_amount, Money::ZERO);

    // Defensive: the user owns the obligation; it is a deficit obligation.
    assert_eq!(ob.user_id, h.user_id);
    assert_eq!(ob.source, ObligationSource::Deficit);
}

// ===========================================================================
// 6. finance_deficit guards
// ===========================================================================

#[tokio::test]
async fn finance_deficit_rejects_a_non_deficit_month() {
    let h = harness(Money::ZERO, Money::from_major(1_000), Decimal::new(75, 2));
    let err = h
        .deficit
        .finance_deficit(
            &h.closed_month,
            &h.next_month,
            h.fund_id,
            3,
            ymd(2026, 8, 1),
            Utc::now(),
        )
        .await;
    assert!(err.is_err(), "financing a non-deficit month must error");
}

#[tokio::test]
async fn finance_deficit_rejects_zero_months() {
    let h = harness(
        Money::from_major(900),
        Money::from_major(1_000),
        Decimal::new(75, 2),
    );
    let err = h
        .deficit
        .finance_deficit(
            &h.closed_month,
            &h.next_month,
            h.fund_id,
            0,
            ymd(2026, 8, 1),
            Utc::now(),
        )
        .await;
    assert!(err.is_err(), "zero-month financing must error");
}

#[tokio::test]
async fn single_month_financing_posts_the_whole_principal_and_settles() {
    // months == 1: installment 1 == the whole principal; the obligation settles
    // immediately (remaining == 0).
    let h = harness(
        Money::from_major(800),
        Money::from_major(900),
        Decimal::new(75, 2),
    );
    let ob = h
        .deficit
        .finance_deficit(
            &h.closed_month,
            &h.next_month,
            h.fund_id,
            1,
            ymd(2026, 8, 1),
            Utc::now(),
        )
        .await
        .expect("finance");
    assert_eq!(ob.remaining_amount, Money::ZERO);
    assert_eq!(ob.status, ObligationStatus::Paid);
    // Next month's Other absorbed the whole $800 as installment 1.
    assert_eq!(oracle_next_month_other_installments(&h), Decimal::from(800));
}
