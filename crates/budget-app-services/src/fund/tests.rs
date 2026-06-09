//! Unit tests for the fund service (`SPEC §4.7`, `§4.9`; D6 / D7).
//!
//! DB-free in-memory fakes per aggregate, mirroring the month-lifecycle test
//! style. The tests assert the two load-bearing invariants:
//!   - a fund contribution / sinking accrual / installment is a manual Other-bucket
//!     expense (`is_fund_draw = false`), so `counts_in_month_expense_remaining`
//!     COUNTS it in the rolling-Other net, reducing it by the contribution
//!     (`BUDGET-FUND-EARMARK-1` / D6 Model A), while a fund DRAW (surplus draw,
//!     sinking payout; `is_fund_draw = true`) is excluded, and
//!   - a buffer-financed full-price transaction is excluded from the month
//!     expense sum (it is referenced by the obligation), while the installments
//!     ARE counted (`BUDGET-NO-DOUBLE-CHARGE-1` / D7).
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
use budget_domain::enums::{
    Cadence, CategoryGrp, FundKind, ObligationSource, ObligationStatus, TransactionSource,
    TransactionStatus,
};
use budget_domain::fund::Fund;
use budget_domain::ids::{
    BudgetId, CategoryId, CategoryKey, FundId, MonthId, RepaymentObligationId, TransactionId,
    UserId,
};
use budget_domain::money::Money;
use budget_domain::predicates::counts_in_month_expense_remaining;
use budget_domain::repayment_obligation::RepaymentObligation;
use budget_domain::repositories::{BudgetRepository, FundRepository, TransactionRepository};
use budget_domain::transaction::Transaction;
use budget_domain::uow::{UnitOfWork, UowFuture, UowProvider};
use budget_domain::{CategorySpent, MonthNet, RepositoryError};

use super::*;

// ---------------------------------------------------------------------------
// Fakes
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
            .filter(|o| o.user_id == user_id && o.status == ObligationStatus::Active)
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

    async fn find_active_deficit_obligation_for_month(
        &self,
        month_id: MonthId,
    ) -> Result<Option<RepaymentObligation>, RepositoryError> {
        let store = self.store.lock().map_err(poisoned)?;
        Ok(store
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
        _user_id: UserId,
        _date: NaiveDate,
    ) -> Result<Option<Budget>, RepositoryError> {
        Ok(None)
    }

    async fn find_current(&self, _user_id: UserId) -> Result<Option<Budget>, RepositoryError> {
        Ok(None)
    }

    async fn list_for_user(&self, _user_id: UserId) -> Result<Vec<Budget>, RepositoryError> {
        Ok(Vec::new())
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
        let mut store = self.store.lock().map_err(poisoned)?;
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

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

struct Harness {
    funds: Arc<FakeFundRepo>,
    transactions: Arc<FakeTransactionRepo>,
    budgets: Arc<FakeBudgetRepo>,
    service: FundService,
    user_id: UserId,
    budget_id: BudgetId,
    month_id: MonthId,
    /// A fund-bound earmark category used as the contribution/installment target.
    earmark_category_id: CategoryId,
}

fn ymd(y: i32, m: u32, d: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, d).unwrap_or(NaiveDate::MIN)
}

fn harness() -> Harness {
    let funds = Arc::new(FakeFundRepo::new());
    let transactions = Arc::new(FakeTransactionRepo::new());
    let budgets = Arc::new(FakeBudgetRepo::new());

    let user_id = UserId::generate();
    let budget_id = BudgetId::generate();
    let month_id = MonthId::generate();
    let earmark_category_id = CategoryId::generate();

    // A fund-bound (annual sinking) earmark category — the contribution and
    // installment exclusion seam (BUDGET-FUND-EARMARK-1).
    {
        let mut store = budgets.store.lock().unwrap();
        store.categories.push(annual_fund_category(
            earmark_category_id,
            budget_id,
            "Buffer earmark",
            Money::from_major(1_200),
        ));
    }

    let service = FundService::new(
        Arc::clone(&funds) as Arc<dyn FundRepository>,
        Arc::clone(&transactions) as Arc<dyn TransactionRepository>,
        Arc::clone(&budgets) as Arc<dyn BudgetRepository>,
        Arc::new(FakeUowProvider) as Arc<dyn UowProvider>,
    );

    Harness {
        funds,
        transactions,
        budgets,
        service,
        user_id,
        budget_id,
        month_id,
        earmark_category_id,
    }
}

fn annual_fund_category(
    id: CategoryId,
    budget_id: BudgetId,
    name: &str,
    amount: Money,
) -> Category {
    Category {
        id,
        budget_id,
        category_key: CategoryKey::generate(),
        name: name.to_owned(),
        amount,
        grp: CategoryGrp::Fixed,
        settle_type: None,
        expected_bills: None,
        is_rollover_bucket: false,
        cadence: Cadence::Annual,
        period_months: None,
        fund_balance: Money::ZERO,
        next_due_date: None,
        sort_order: 1,
    }
}

fn push_fund(h: &Harness, fund: Fund) {
    h.funds.store.lock().unwrap().funds.push(fund);
}

fn buffer_fund(h: &Harness, balance: Money, target: Money) -> Fund {
    Fund {
        id: FundId::generate(),
        user_id: h.user_id,
        name: "Buffer".to_owned(),
        kind: FundKind::Buffer,
        balance,
        target_balance: Some(target),
        compulsory_repayment: true,
        created_at: Utc::now(),
    }
}

fn surplus_fund(h: &Harness, balance: Money) -> Fund {
    Fund {
        id: FundId::generate(),
        user_id: h.user_id,
        name: "Vacation".to_owned(),
        kind: FundKind::Surplus,
        balance,
        target_balance: None,
        compulsory_repayment: false,
        created_at: Utc::now(),
    }
}

/// The fund-category id set for the harness budget (what the netting excludes).
fn fund_category_ids(h: &Harness) -> Vec<CategoryId> {
    let store = h.budgets.store.lock().unwrap();
    store
        .categories
        .iter()
        .filter(|c| c.is_sinking_fund())
        .map(|c| c.id)
        .collect()
}

fn now() -> chrono::DateTime<Utc> {
    Utc::now()
}

// ===========================================================================
// 1. Contributions (BUDGET-FUND-EARMARK-1 / D6)
// ===========================================================================

#[tokio::test]
async fn contribution_increments_balance_and_posts_counted_expense() {
    let h = harness();
    let fund = surplus_fund(&h, Money::from_major(0));
    let fund_id = fund.id;
    push_fund(&h, fund);

    let updated = h
        .service
        .contribute(
            fund_id,
            h.month_id,
            h.earmark_category_id,
            Money::from_major(300),
            ymd(2026, 6, 1),
            now(),
        )
        .await
        .expect("contribute");

    // Balance went up by the contribution.
    assert_eq!(updated.balance, Money::from_major(300));

    // Exactly one transaction posted: a -$300 expense on the Other-bucket category.
    let txns = h.transactions.all();
    assert_eq!(txns.len(), 1);
    let c = &txns[0];
    assert_eq!(c.amount, Money::from_major(-300));
    assert_eq!(c.category_id, Some(h.earmark_category_id));
    // D6 Model A: a contribution is NOT a fund draw — it counts.
    assert!(!c.is_fund_draw, "a contribution is not a fund draw");

    // BUDGET-FUND-EARMARK-1 (D6 Model A): it COUNTS in the rolling-Other expense
    // sum, reducing the net by the contribution.
    let fund_cats = fund_category_ids(&h);
    assert!(
        counts_in_month_expense_remaining(c, &fund_cats, &[]),
        "fund contribution must COUNT in the rolling-Other net (D6 Model A)"
    );
}

#[tokio::test]
async fn contribution_rejects_non_positive_amount() {
    let h = harness();
    let fund = surplus_fund(&h, Money::ZERO);
    let fund_id = fund.id;
    push_fund(&h, fund);

    let err = h
        .service
        .contribute(
            fund_id,
            h.month_id,
            h.earmark_category_id,
            Money::from_major(-50),
            ymd(2026, 6, 1),
            now(),
        )
        .await
        .expect_err("negative contribution rejected");
    assert!(matches!(err, DomainError::Invariant(_)));
}

// ===========================================================================
// 2. Large purchases (D7)
// ===========================================================================

#[tokio::test]
async fn buffer_financed_full_price_is_excluded_but_installments_count() {
    // SPEC §4.9 D7: a $1,200 buffer-financed purchase over 12 months.
    let h = harness();
    let fund = buffer_fund(&h, Money::from_major(5_000), Money::from_major(5_000));
    let fund_id = fund.id;
    push_fund(&h, fund);

    let txn_id = h
        .service
        .record_large_purchase(
            h.user_id,
            h.month_id,
            h.earmark_category_id,
            Money::from_major(1_200),
            "MacBook".to_owned(),
            ymd(2026, 6, 5),
            LargePurchaseResolution::BufferFinanced {
                fund_id,
                months: 12,
            },
            now(),
        )
        .await
        .expect("buffer finance");

    // The buffer was drawn down to front the cash.
    let buffer = h.funds.find_by_id(fund_id).await.unwrap().unwrap();
    assert_eq!(buffer.balance, Money::from_major(3_800));

    // An obligation was created: $1,200 total, $100/mo x 12.
    let obligation = h
        .funds
        .find_obligation_for_transaction(txn_id)
        .await
        .unwrap()
        .expect("obligation exists");
    assert_eq!(obligation.total_amount, Money::from_major(1_200));
    assert_eq!(obligation.remaining_amount, Money::from_major(1_200));
    assert_eq!(obligation.installment_amount, Money::from_major(100));
    assert_eq!(obligation.months_remaining, 12);
    assert_eq!(obligation.status, ObligationStatus::Active);

    // The full-price tracking transaction is EXCLUDED from the month expense
    // remaining (it is referenced by the obligation) — this is what stops the
    // full price from blowing up its month.
    let full_price = h.transactions.find_by_id(txn_id).await.unwrap().unwrap();
    assert_eq!(full_price.amount, Money::from_major(-1_200));
    let fund_cats = fund_category_ids(&h);
    let buffer_financed = vec![txn_id];
    assert!(
        !counts_in_month_expense_remaining(&full_price, &fund_cats, &buffer_financed),
        "buffer-financed full price must be excluded from the month expense sum"
    );

    // Post one installment: it IS a counted month-budget expense (D6 Model A —
    // is_fund_draw=false, so it reduces the rolling-Other net) AND it restores the
    // buffer and decrements the obligation.
    let after = h
        .service
        .post_installment(
            obligation.id,
            h.month_id,
            h.earmark_category_id,
            ymd(2026, 7, 1),
            now(),
        )
        .await
        .expect("installment");
    assert_eq!(after.remaining_amount, Money::from_major(1_100));
    assert_eq!(after.months_remaining, 11);
    let buffer = h.funds.find_by_id(fund_id).await.unwrap().unwrap();
    assert_eq!(buffer.balance, Money::from_major(3_900));

    // The installment row COUNTS in the rolling-Other net (D6 Model A).
    let installment = h
        .transactions
        .all()
        .into_iter()
        .find(|t| t.description == "Buffer repayment installment")
        .expect("installment txn");
    assert!(
        !installment.is_fund_draw,
        "an installment is a contribution back into the buffer, not a draw"
    );
    assert!(
        counts_in_month_expense_remaining(&installment, &fund_cats, &buffer_financed),
        "buffer-repayment installment must COUNT in the rolling-Other net (D6 Model A)"
    );
}

#[tokio::test]
async fn buffer_repayment_runs_to_paid_with_exact_cents() {
    // An awkward total that does not divide evenly: $100.00 over 3 months =>
    // $33.33 x 2 + $33.34 final, summing to exactly $100.00.
    let h = harness();
    let fund = buffer_fund(&h, Money::from_major(1_000), Money::from_major(1_000));
    let fund_id = fund.id;
    push_fund(&h, fund);

    let txn_id = h
        .service
        .record_large_purchase(
            h.user_id,
            h.month_id,
            h.earmark_category_id,
            Money::from_major(100),
            "Gadget".to_owned(),
            ymd(2026, 6, 5),
            LargePurchaseResolution::BufferFinanced { fund_id, months: 3 },
            now(),
        )
        .await
        .expect("finance");

    let obligation = h
        .funds
        .find_obligation_for_transaction(txn_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(obligation.installment_amount, Money::from_minor(3_333));

    // Pay all three installments.
    let mut last = obligation.clone();
    for _ in 0..3 {
        last = h
            .service
            .post_installment(
                last.id,
                h.month_id,
                h.earmark_category_id,
                ymd(2026, 7, 1),
                now(),
            )
            .await
            .expect("installment");
    }
    assert_eq!(last.status, ObligationStatus::Paid);
    assert_eq!(last.remaining_amount, Money::ZERO);
    assert_eq!(last.months_remaining, 0);

    // The buffer is restored to exactly its pre-purchase balance — zero cents lost.
    let buffer = h.funds.find_by_id(fund_id).await.unwrap().unwrap();
    assert_eq!(buffer.balance, Money::from_major(1_000));

    // A fourth installment is rejected (already paid).
    let err = h
        .service
        .post_installment(
            last.id,
            h.month_id,
            h.earmark_category_id,
            ymd(2026, 10, 1),
            now(),
        )
        .await
        .expect_err("paid obligation rejects further installments");
    assert!(matches!(err, DomainError::IllegalState(_)));
}

#[tokio::test]
async fn pay_through_surplus_draws_fund_with_no_obligation() {
    let h = harness();
    let fund = surplus_fund(&h, Money::from_major(2_000));
    let fund_id = fund.id;
    push_fund(&h, fund);

    let txn_id = h
        .service
        .record_large_purchase(
            h.user_id,
            h.month_id,
            h.earmark_category_id,
            Money::from_major(800),
            "Flights".to_owned(),
            ymd(2026, 6, 5),
            LargePurchaseResolution::PayThroughSurplus(fund_id),
            now(),
        )
        .await
        .expect("surplus draw");

    // Fund drawn down; NO obligation created.
    let fund = h.funds.find_by_id(fund_id).await.unwrap().unwrap();
    assert_eq!(fund.balance, Money::from_major(1_200));
    assert!(
        h.funds
            .find_obligation_for_transaction(txn_id)
            .await
            .unwrap()
            .is_none(),
        "surplus draw creates no repayment obligation"
    );

    // The draw is a fund DRAW (is_fund_draw=true) -> excluded from the net (not a
    // re-charged budget expense; BUDGET-NO-DOUBLE-CHARGE-1 / D6 Model A — the money
    // was already expensed by the surplus contributions, which count).
    let draw = h.transactions.find_by_id(txn_id).await.unwrap().unwrap();
    assert!(draw.is_fund_draw, "a surplus draw is a fund draw");
    let fund_cats = fund_category_ids(&h);
    assert!(!counts_in_month_expense_remaining(&draw, &fund_cats, &[]));
}

#[tokio::test]
async fn buffer_financed_requires_a_buffer_fund() {
    let h = harness();
    let fund = surplus_fund(&h, Money::from_major(2_000));
    let fund_id = fund.id;
    push_fund(&h, fund);

    let err = h
        .service
        .record_large_purchase(
            h.user_id,
            h.month_id,
            h.earmark_category_id,
            Money::from_major(800),
            "x".to_owned(),
            ymd(2026, 6, 5),
            LargePurchaseResolution::BufferFinanced { fund_id, months: 6 },
            now(),
        )
        .await
        .expect_err("surplus fund cannot be buffer-financed");
    assert!(matches!(err, DomainError::Invariant(_)));
}

// ===========================================================================
// 3. Sinking funds (SPEC §4.7)
// ===========================================================================

#[tokio::test]
async fn sinking_accrual_adds_monthly_share_and_counts_in_net() {
    // $1,200 / 12 = $100 accrued into fund_balance.
    let h = harness();
    let updated = h
        .service
        .accrue_sinking_fund(
            h.earmark_category_id,
            h.month_id,
            h.user_id,
            ymd(2026, 6, 1),
            now(),
        )
        .await
        .expect("accrue");
    assert_eq!(updated.fund_balance, Money::from_major(100));

    let txns = h.transactions.all();
    assert_eq!(txns.len(), 1);
    assert_eq!(txns[0].amount, Money::from_major(-100));
    // D6 Model A: the accrual is a contribution, not a draw -> it COUNTS in the net.
    assert!(
        !txns[0].is_fund_draw,
        "a sinking accrual is not a fund draw"
    );
    let fund_cats = fund_category_ids(&h);
    assert!(counts_in_month_expense_remaining(&txns[0], &fund_cats, &[]));
}

#[tokio::test]
async fn tag_sinking_payout_draws_reserve_and_resets_clock_forward() {
    // Accrue twice ($200 reserve), then tag a $180 bill as the payout.
    let h = harness();
    h.service
        .accrue_sinking_fund(
            h.earmark_category_id,
            h.month_id,
            h.user_id,
            ymd(2026, 6, 1),
            now(),
        )
        .await
        .expect("accrue 1");
    h.service
        .accrue_sinking_fund(
            h.earmark_category_id,
            h.month_id,
            h.user_id,
            ymd(2026, 7, 1),
            now(),
        )
        .await
        .expect("accrue 2");

    // The real bill arrives as an uncategorized transaction.
    let bill = Transaction {
        id: TransactionId::generate(),
        user_id: h.user_id,
        month_id: h.month_id,
        category_id: None,
        account_id: None,
        date: ymd(2026, 7, 15),
        amount: Money::from_major(-180),
        description: "Insurance".to_owned(),
        source: TransactionSource::Manual,
        plaid_transaction_id: None,
        status: TransactionStatus::Settled,
        income_kind: None,
        is_rollover: false,
        is_fund_draw: false,
        matched_transaction_id: None,
        created_at: now(),
        updated_at: now(),
    };
    let bill_id = bill.id;
    h.transactions.store.lock().unwrap().txns.push(bill);

    let updated = h
        .service
        .tag_sinking_payout(
            h.earmark_category_id,
            bill_id,
            Money::from_major(180),
            ymd(2026, 7, 15),
            now(),
        )
        .await
        .expect("tag payout");

    // Reserve drawn down: $200 - $180 = $20.
    assert_eq!(updated.fund_balance, Money::from_major(20));
    // Reset-on-payment: next_due_date re-anchored one annual period forward from
    // the payment date (forward-looking accrual, SPEC §4.7).
    assert_eq!(updated.next_due_date, Some(ymd(2027, 7, 15)));

    // The bill was reassigned to the sinking category AND marked a fund DRAW ->
    // excluded from the net (D6 Model A: the reserve, built from already-counted
    // accrual contributions, covers it; BUDGET-NO-DOUBLE-CHARGE-1).
    let tagged = h.transactions.find_by_id(bill_id).await.unwrap().unwrap();
    assert_eq!(tagged.category_id, Some(h.earmark_category_id));
    assert!(tagged.is_fund_draw, "a sinking payout is a fund draw");
    let fund_cats = fund_category_ids(&h);
    assert!(!counts_in_month_expense_remaining(&tagged, &fund_cats, &[]));
}

#[tokio::test]
async fn accrue_rejects_non_sinking_category() {
    let h = harness();
    // A plain monthly category is not a sinking fund.
    let monthly = CategoryId::generate();
    {
        let mut store = h.budgets.store.lock().unwrap();
        store.categories.push(Category {
            id: monthly,
            budget_id: h.budget_id,
            category_key: CategoryKey::generate(),
            name: "Groceries".to_owned(),
            amount: Money::from_major(500),
            grp: CategoryGrp::Discretionary,
            settle_type: None,
            expected_bills: None,
            is_rollover_bucket: false,
            cadence: Cadence::Monthly,
            period_months: None,
            fund_balance: Money::ZERO,
            next_due_date: None,
            sort_order: 5,
        });
    }
    let err = h
        .service
        .accrue_sinking_fund(monthly, h.month_id, h.user_id, ymd(2026, 6, 1), now())
        .await
        .expect_err("monthly category is not a sinking fund");
    assert!(matches!(err, DomainError::Invariant(_)));
}

// ===========================================================================
// 4. Buffer health — advisory (SPEC §4.9)
// ===========================================================================

#[test]
fn buffer_health_above_target_flags_excess() {
    let h = harness();
    let fund = buffer_fund(&h, Money::from_major(6_000), Money::from_major(5_000));
    assert_eq!(
        FundService::buffer_health(&fund, false),
        BufferHealth::AboveTarget(Money::from_major(1_000))
    );
}

#[test]
fn buffer_health_below_target_with_obligations_flags_caution() {
    let h = harness();
    let fund = buffer_fund(&h, Money::from_major(3_000), Money::from_major(5_000));
    assert_eq!(
        FundService::buffer_health(&fund, true),
        BufferHealth::BelowTargetWithObligations(Money::from_major(2_000))
    );
    // Same shortfall, no obligations -> the softer below-target verdict.
    assert_eq!(
        FundService::buffer_health(&fund, false),
        BufferHealth::BelowTarget(Money::from_major(2_000))
    );
}

#[test]
fn buffer_health_on_target_and_non_buffer_are_neutral() {
    let h = harness();
    let on = buffer_fund(&h, Money::from_major(5_000), Money::from_major(5_000));
    assert_eq!(
        FundService::buffer_health(&on, true),
        BufferHealth::OnTarget
    );

    let surplus = surplus_fund(&h, Money::from_major(9_999));
    assert_eq!(
        FundService::buffer_health(&surplus, true),
        BufferHealth::OnTarget,
        "non-buffer funds never flag"
    );
}

#[tokio::test]
async fn buffer_health_for_reads_obligations() {
    let h = harness();
    let fund = buffer_fund(&h, Money::from_major(3_000), Money::from_major(5_000));
    let fund_id = fund.id;
    push_fund(&h, fund);

    // No obligations yet -> soft below-target.
    assert_eq!(
        h.service.buffer_health_for(fund_id).await.expect("health"),
        BufferHealth::BelowTarget(Money::from_major(2_000))
    );

    // Add an active obligation -> caution.
    h.funds
        .store
        .lock()
        .unwrap()
        .obligations
        .push(RepaymentObligation {
            id: RepaymentObligationId::generate(),
            user_id: h.user_id,
            fund_id,
            source: ObligationSource::LargePurchase,
            transaction_id: Some(TransactionId::generate()),
            origin_month_id: None,
            total_amount: Money::from_major(500),
            remaining_amount: Money::from_major(500),
            installment_amount: Money::from_major(100),
            months_remaining: 5,
            status: ObligationStatus::Active,
            created_at: Utc::now(),
        });
    assert_eq!(
        h.service.buffer_health_for(fund_id).await.expect("health"),
        BufferHealth::BelowTargetWithObligations(Money::from_major(2_000))
    );
}
