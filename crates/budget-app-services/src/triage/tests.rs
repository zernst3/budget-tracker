//! Adversarial tests for the pending-triage flow (`SPEC §7`, `§4.4`, `§4.9`,
//! BACKEND-3).
//!
//! Break-don't-confirm, with INDEPENDENT `rust_decimal` oracles. The fakes are
//! DB-free in-memory stores (the established app-services test style); the build's
//! own arithmetic is never used to check itself — each money assertion is
//! cross-checked against a fold/branch re-derived inline. The properties proven:
//!
//!   1. **`pay_directly` is a plain expense** — the row keeps its own amount,
//!      `is_fund_draw = false`, gets a category, and COUNTS exactly once in the
//!      month net (`counts_in_month_expense_remaining`); no fund is touched.
//!   2. **`pay_from_savings` counts the money exactly once** — the row becomes a fund
//!      DRAW (`is_fund_draw = true`, EXCLUDED from the net,
//!      `BUDGET-NO-DOUBLE-CHARGE-1`), and the fund balance drops by EXACTLY the
//!      amount (no double-charge: not also a month expense).
//!   3. **buffer-financed posts zero net month impact + an obligation** (D7) — the
//!      tracking row stays `is_fund_draw = false` but is EXCLUDED via the obligation
//!      list, an obligation of `total = price` / `installment = price/months` is
//!      created, and the buffer is drawn down once. The in-month budget effect is
//!      ZERO (the installments, posted later, are the budget effect).
//!   4. **a Plaid `pending` charge never enters the inbox** (`SPEC §4.4`) — and
//!      triaging one (or an already-categorized row) is rejected, never mutating.
//!   5. **triage is atomic and removes the row from the inbox** — after a successful
//!      triage the row has a category and no longer appears in `pending_inbox`.

#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]
#![allow(clippy::panic)]

use std::any::Any;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::{NaiveDate, Utc};
use rust_decimal::Decimal;

use budget_domain::budget::Budget;
use budget_domain::category::Category;
use budget_domain::enums::{
    FundKind, ObligationSource, ObligationStatus, TransactionSource, TransactionStatus,
};
use budget_domain::error::DomainError;
use budget_domain::fund::Fund;
use budget_domain::ids::{
    BudgetId, CategoryId, FundId, MonthId, RepaymentObligationId, TransactionId, UserId,
};
use budget_domain::money::Money;
use budget_domain::predicates::counts_in_month_expense_remaining;
use budget_domain::repayment_obligation::RepaymentObligation;
use budget_domain::repositories::{BudgetRepository, FundRepository, TransactionRepository};
use budget_domain::transaction::Transaction;
use budget_domain::uow::{UnitOfWork, UowFuture, UowProvider};
use budget_domain::{CategorySpent, RepositoryError};

use crate::fund::FundService;

use super::{Treatment, TriageInput, TriageService};

// ---------------------------------------------------------------------------
// Fakes (DB-free, in-memory)
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
    fn fund(&self, id: FundId) -> Fund {
        self.store
            .lock()
            .unwrap()
            .funds
            .iter()
            .find(|f| f.id == id)
            .cloned()
            .expect("fund present")
    }
    fn obligations(&self) -> Vec<RepaymentObligation> {
        self.store.lock().unwrap().obligations.clone()
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
    async fn find_deficit_obligation_for_month(
        &self,
        month_id: MonthId,
    ) -> Result<Option<RepaymentObligation>, RepositoryError> {
        let store = self.store.lock().map_err(poisoned)?;
        Ok(store
            .obligations
            .iter()
            .find(|o| o.origin_month_id == Some(month_id) && o.source == ObligationSource::Deficit)
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
    fn insert(&self, t: Transaction) {
        self.store.lock().unwrap().txns.push(t);
    }
    fn get(&self, id: TransactionId) -> Transaction {
        self.store
            .lock()
            .unwrap()
            .txns
            .iter()
            .find(|t| t.id == id)
            .cloned()
            .expect("txn present")
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
        user_id: UserId,
    ) -> Result<Vec<Transaction>, RepositoryError> {
        // The real query (SPEC §7 / §4.11): settled + uncategorized + NOT a transfer
        // + this user, oldest first. The `Settled` filter excludes Plaid `pending`
        // rows (SPEC §4.4); the `!is_transfer` filter is what lets a triaged transfer
        // LEAVE the inbox via is_transfer=true (BUDGET-TRANSFER-EXCLUDE-1).
        let store = self.store.lock().map_err(poisoned)?;
        let mut rows: Vec<Transaction> = store
            .txns
            .iter()
            .filter(|t| {
                t.user_id == user_id
                    && t.status == TransactionStatus::Settled
                    && t.category_id.is_none()
                    && !t.is_transfer
            })
            .cloned()
            .collect();
        rows.sort_by_key(|t| t.date);
        Ok(rows)
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

// A budget repo fake (unused by triage directly, but FundService needs one).
struct FakeBudgetRepo;

#[async_trait]
impl BudgetRepository for FakeBudgetRepo {
    async fn find_by_id(&self, _id: BudgetId) -> Result<Option<Budget>, RepositoryError> {
        Ok(None)
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
    async fn list_categories(
        &self,
        _budget_id: BudgetId,
    ) -> Result<Vec<Category>, RepositoryError> {
        Ok(Vec::new())
    }
    async fn find_category(&self, _id: CategoryId) -> Result<Option<Category>, RepositoryError> {
        Ok(None)
    }
    async fn find_rollover_bucket(
        &self,
        _budget_id: BudgetId,
    ) -> Result<Option<Category>, RepositoryError> {
        Ok(None)
    }
    async fn save(
        &self,
        _budget: &Budget,
        _uow: Option<&dyn UnitOfWork>,
    ) -> Result<(), RepositoryError> {
        Ok(())
    }
    async fn save_category(
        &self,
        _category: &Category,
        _uow: Option<&dyn UnitOfWork>,
    ) -> Result<(), RepositoryError> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Harness + fixtures
// ---------------------------------------------------------------------------

struct Harness {
    funds: Arc<FakeFundRepo>,
    transactions: Arc<FakeTransactionRepo>,
    triage: TriageService,
    user_id: UserId,
    month_id: MonthId,
}

fn harness() -> Harness {
    let funds = Arc::new(FakeFundRepo::new());
    let transactions = Arc::new(FakeTransactionRepo::new());
    let budgets = Arc::new(FakeBudgetRepo);

    let fund_service = Arc::new(FundService::new(
        Arc::clone(&funds) as Arc<dyn FundRepository>,
        Arc::clone(&transactions) as Arc<dyn TransactionRepository>,
        budgets as Arc<dyn BudgetRepository>,
        Arc::new(FakeUowProvider) as Arc<dyn UowProvider>,
    ));

    let triage = TriageService::new(
        Arc::clone(&transactions) as Arc<dyn TransactionRepository>,
        Arc::clone(&fund_service),
        Arc::new(FakeUowProvider) as Arc<dyn UowProvider>,
    );

    Harness {
        funds,
        transactions,
        triage,
        user_id: UserId::generate(),
        month_id: MonthId::generate(),
    }
}

fn ymd(y: i32, m: u32, d: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, d).unwrap_or(NaiveDate::MIN)
}

/// A settled, uncategorized bank charge — an inbox row.
fn settled_uncategorized(user_id: UserId, month_id: MonthId, amount: Money) -> Transaction {
    let now = Utc::now();
    Transaction {
        id: TransactionId::generate(),
        user_id,
        month_id,
        category_id: None,
        account_id: None,
        date: ymd(2026, 7, 15),
        amount,
        description: "MERCHANT".to_owned(),
        source: TransactionSource::Plaid,
        plaid_transaction_id: Some("plaid-1".to_owned()),
        status: TransactionStatus::Settled,
        income_kind: None,
        is_rollover: false,
        is_fund_draw: false,
        matched_transaction_id: None,
        comment: None,
        is_transfer: false,
        plaid_category: None,
        created_at: now,
        updated_at: now,
    }
}

fn surplus_fund(user_id: UserId, balance: Money) -> Fund {
    Fund {
        id: FundId::generate(),
        user_id,
        name: "Vacation surplus".to_owned(),
        kind: FundKind::Surplus,
        balance,
        target_balance: None,
        compulsory_repayment: false,
        created_at: Utc::now(),
    }
}

fn buffer_fund(user_id: UserId, balance: Money) -> Fund {
    Fund {
        id: FundId::generate(),
        user_id,
        name: "Buffer".to_owned(),
        kind: FundKind::Buffer,
        balance,
        target_balance: Some(Money::from_major(15_000)),
        compulsory_repayment: true,
        created_at: Utc::now(),
    }
}

/// Independent oracle: does `counts_in_month_expense_remaining` count this row?
/// Re-implements the predicate's intent inline (income / fund-draw / buffer-financed
/// excluded) so a test cannot tautologically green against the build's own predicate
/// — we call the predicate AND assert the oracle agrees.
fn oracle_counts(t: &Transaction, buffer_financed_ids: &[TransactionId]) -> bool {
    if t.is_income() {
        return false;
    }
    if t.is_fund_draw {
        return false;
    }
    if buffer_financed_ids.contains(&t.id) {
        return false;
    }
    true
}

// ---------------------------------------------------------------------------
// 1. pay_directly — a plain in-month expense, counted exactly once
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pay_directly_is_a_plain_counted_expense_no_fund_touched() {
    let h = harness();
    let cat = CategoryId::generate();
    let amount = Money::from_minor(-4_250); // -$42.50
    let txn = settled_uncategorized(h.user_id, h.month_id, amount);
    let txn_id = txn.id;
    h.transactions.insert(txn);

    let out = h
        .triage
        .triage(
            TriageInput {
                transaction_id: txn_id,
                category_id: Some(cat),
                comment: Some("lunch".to_owned()),
                treatment: Treatment::PayDirectly,
            },
            Utc::now(),
        )
        .await
        .expect("triage ok");
    assert_eq!(
        out.obligation_id, None,
        "pay_directly creates no obligation"
    );

    let saved = h.transactions.get(txn_id);
    assert_eq!(saved.category_id, Some(cat), "category assigned");
    assert_eq!(saved.comment.as_deref(), Some("lunch"), "comment assigned");
    assert!(!saved.is_fund_draw, "pay_directly is NOT a fund draw");
    assert_eq!(saved.amount, amount, "amount unchanged");

    // Counts EXACTLY ONCE in the month net: predicate true, oracle agrees, and the
    // counted amount is the row's own amount (not doubled).
    assert!(counts_in_month_expense_remaining(&saved, &[], &[]));
    assert!(oracle_counts(&saved, &[]));
    let counted: Decimal = [saved.amount]
        .iter()
        .filter(|_| counts_in_month_expense_remaining(&saved, &[], &[]))
        .map(Money::as_decimal)
        .sum();
    assert_eq!(counted, Decimal::new(-4250, 2), "counted once = -$42.50");

    // No fund / obligation touched.
    assert!(h.funds.obligations().is_empty());
}

// ---------------------------------------------------------------------------
// 2. pay_from_savings — a fund draw, counted exactly once (no double-charge)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pay_from_savings_draws_the_fund_once_and_excludes_from_net() {
    let h = harness();
    let fund = surplus_fund(h.user_id, Money::from_major(1_000));
    let fund_id = fund.id;
    h.funds.store.lock().unwrap().funds.push(fund);

    let cat = CategoryId::generate();
    let amount = Money::from_minor(-30_000); // -$300 purchase pre-saved
    let txn = settled_uncategorized(h.user_id, h.month_id, amount);
    let txn_id = txn.id;
    h.transactions.insert(txn);

    h.triage
        .triage(
            TriageInput {
                transaction_id: txn_id,
                category_id: Some(cat),
                comment: None,
                treatment: Treatment::PayFromSavings { fund_id },
            },
            Utc::now(),
        )
        .await
        .expect("triage ok");

    let saved = h.transactions.get(txn_id);
    assert_eq!(saved.category_id, Some(cat));
    assert!(saved.is_fund_draw, "fund draw flagged");

    // EXCLUDED from the month net (BUDGET-NO-DOUBLE-CHARGE-1): not a re-charged
    // expense — the money was already expensed when contributed.
    assert!(!counts_in_month_expense_remaining(&saved, &[], &[]));
    assert!(!oracle_counts(&saved, &[]));

    // The fund dropped by EXACTLY the purchase magnitude — counted once. Oracle:
    // 1000 - 300 = 700, re-derived independently of the service arithmetic.
    let fund_after = h.funds.fund(fund_id);
    let oracle_balance = Decimal::from(1_000) - Decimal::from(300);
    assert_eq!(fund_after.balance.as_decimal(), oracle_balance);
    assert_eq!(fund_after.balance, Money::from_major(700));

    // No obligation (surplus draw has no repayment).
    assert!(h.funds.obligations().is_empty());
}

// ---------------------------------------------------------------------------
// 3. spread_over_months — buffer-financed: zero net month impact + an obligation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn spread_over_months_posts_zero_net_month_impact_plus_an_obligation() {
    let h = harness();
    let buffer = buffer_fund(h.user_id, Money::from_major(15_000));
    let fund_id = buffer.id;
    h.funds.store.lock().unwrap().funds.push(buffer);

    let cat = CategoryId::generate();
    let price = Money::from_major(2_400); // a $2,400 laptop
    let txn = settled_uncategorized(h.user_id, h.month_id, Money::from_major(-2_400));
    let txn_id = txn.id;
    h.transactions.insert(txn);

    let out = h
        .triage
        .triage(
            TriageInput {
                transaction_id: txn_id,
                category_id: Some(cat),
                comment: Some("MacBook".to_owned()),
                treatment: Treatment::SpreadOverMonths { fund_id, months: 6 },
            },
            Utc::now(),
        )
        .await
        .expect("triage ok");
    let obligation_id = out.obligation_id.expect("obligation created");

    // The tracking row: categorized, is_fund_draw = false, but EXCLUDED from the net
    // via the obligation list (D7) — so its ZERO net month impact comes from the
    // exclusion, not the flag.
    let saved = h.transactions.get(txn_id);
    assert_eq!(saved.category_id, Some(cat));
    assert!(!saved.is_fund_draw, "tracking row is not a fund-draw");

    // The obligation IS the buffer-financed exclusion key.
    let obligations = h.funds.obligations();
    assert_eq!(obligations.len(), 1);
    let ob = &obligations[0];
    assert_eq!(ob.id, obligation_id);
    assert_eq!(ob.transaction_id, Some(txn_id));
    assert_eq!(ob.source, ObligationSource::LargePurchase);
    assert_eq!(ob.status, ObligationStatus::Active);
    assert_eq!(ob.total_amount, price);
    assert_eq!(ob.remaining_amount, price);
    assert_eq!(ob.months_remaining, 6);
    // installment = 2400 / 6 = 400, independent oracle.
    let oracle_installment = Decimal::from(2_400) / Decimal::from(6);
    assert_eq!(ob.installment_amount.as_decimal(), oracle_installment);
    assert_eq!(ob.installment_amount, Money::from_major(400));

    // ZERO net month impact: with the row in the buffer-financed exclusion set, the
    // predicate excludes it. Both the build predicate AND the oracle agree it does
    // NOT count this month.
    let buffer_ids = vec![txn_id];
    assert!(!oracle_counts(&saved, &buffer_ids));
    // The build predicate agrees: with the tracking row in the buffer-financed
    // exclusion set, it is excluded from the in-month expense remaining (D7).
    assert!(!counts_in_month_expense_remaining(&saved, &[], &buffer_ids));
    // ... and absent the exclusion set it WOULD count (proving the exclusion, not a
    // status/flag quirk, is what zeroes the month impact).
    assert!(counts_in_month_expense_remaining(&saved, &[], &[]));
    // The full-price tracking row is excluded from the in-month sum (D7): the only
    // budget effect would be the LATER installments, none posted yet -> net 0.
    let in_month: Decimal = [saved.clone()]
        .iter()
        .filter(|t| oracle_counts(t, &buffer_ids))
        .map(|t| t.amount.as_decimal())
        .sum();
    assert_eq!(in_month, Decimal::ZERO, "zero net month impact this month");

    // The buffer fronted the cash: balance dropped by exactly the price (15000 -
    // 2400 = 12600), re-derived independently.
    let buffer_after = h.funds.fund(fund_id);
    assert_eq!(
        buffer_after.balance.as_decimal(),
        Decimal::from(15_000) - Decimal::from(2_400)
    );
}

#[tokio::test]
async fn spread_over_months_final_installment_absorbs_the_rounding_remainder() {
    // $100 over 3 months -> 33.33 x 2 + 33.34 == 100 exactly (the obligation tracks
    // the full principal; the clamp lives in post_installment, but the obligation's
    // installment_amount must be the rounded-down figure so the sum reconciles).
    let h = harness();
    let buffer = buffer_fund(h.user_id, Money::from_major(15_000));
    let fund_id = buffer.id;
    h.funds.store.lock().unwrap().funds.push(buffer);

    let txn = settled_uncategorized(h.user_id, h.month_id, Money::from_major(-100));
    let txn_id = txn.id;
    h.transactions.insert(txn);

    h.triage
        .triage(
            TriageInput {
                transaction_id: txn_id,
                category_id: Some(CategoryId::generate()),
                comment: None,
                treatment: Treatment::SpreadOverMonths { fund_id, months: 3 },
            },
            Utc::now(),
        )
        .await
        .expect("triage ok");

    let ob = h.funds.obligations().pop().expect("obligation");
    assert_eq!(ob.total_amount, Money::from_major(100));
    // 100 / 3 rounded to cents = 33.33; the remainder lands on the last installment
    // in post_installment (proven in the fund suite). Here we assert the obligation
    // principal is exact and the installment is the rounded figure.
    assert_eq!(ob.installment_amount, Money::from_minor(3_333));
}

// ---------------------------------------------------------------------------
// 4. Plaid `pending` never enters the inbox; cannot be triaged
// ---------------------------------------------------------------------------

#[tokio::test]
async fn plaid_pending_charge_never_enters_the_inbox() {
    let h = harness();
    // A settled-uncategorized row (belongs in the inbox).
    let settled = settled_uncategorized(h.user_id, h.month_id, Money::from_minor(-1_000));
    let settled_id = settled.id;
    h.transactions.insert(settled);

    // A Plaid `pending` charge (status='pending'): excluded from the inbox by §4.4.
    let mut pending = settled_uncategorized(h.user_id, h.month_id, Money::from_minor(-9_999));
    pending.status = TransactionStatus::Pending;
    pending.plaid_transaction_id = Some("plaid-pending".to_owned());
    let pending_id = pending.id;
    h.transactions.insert(pending);

    let inbox = h.triage.pending_inbox(h.user_id).await.expect("inbox ok");
    let ids: Vec<TransactionId> = inbox.iter().map(|p| p.id).collect();
    assert!(ids.contains(&settled_id), "settled row IS in the inbox");
    assert!(
        !ids.contains(&pending_id),
        "Plaid pending charge is NOT in the inbox (§4.4)"
    );
    assert_eq!(inbox.len(), 1);
}

#[tokio::test]
async fn triaging_a_plaid_pending_charge_is_rejected_and_never_mutates() {
    let h = harness();
    let mut pending = settled_uncategorized(h.user_id, h.month_id, Money::from_minor(-5_000));
    pending.status = TransactionStatus::Pending;
    let pending_id = pending.id;
    h.transactions.insert(pending);

    let err = h
        .triage
        .triage(
            TriageInput {
                transaction_id: pending_id,
                category_id: Some(CategoryId::generate()),
                comment: None,
                treatment: Treatment::PayDirectly,
            },
            Utc::now(),
        )
        .await
        .expect_err("triaging a pending charge must be rejected");
    assert!(matches!(err, DomainError::IllegalState(_)));

    // Unmutated: still pending, still uncategorized.
    let after = h.transactions.get(pending_id);
    assert_eq!(after.status, TransactionStatus::Pending);
    assert!(after.category_id.is_none());
}

// ---------------------------------------------------------------------------
// 5. atomicity + leaves the inbox; double-triage guard
// ---------------------------------------------------------------------------

#[tokio::test]
async fn triaged_row_leaves_the_inbox() {
    let h = harness();
    let txn = settled_uncategorized(h.user_id, h.month_id, Money::from_minor(-2_500));
    let txn_id = txn.id;
    h.transactions.insert(txn);

    assert_eq!(
        h.triage.pending_inbox(h.user_id).await.unwrap().len(),
        1,
        "row starts in the inbox"
    );

    h.triage
        .triage(
            TriageInput {
                transaction_id: txn_id,
                category_id: Some(CategoryId::generate()),
                comment: None,
                treatment: Treatment::PayDirectly,
            },
            Utc::now(),
        )
        .await
        .expect("triage ok");

    let inbox = h.triage.pending_inbox(h.user_id).await.unwrap();
    assert!(
        inbox.is_empty(),
        "after triage the categorized row has left the inbox"
    );
}

#[tokio::test]
async fn double_triage_is_rejected() {
    let h = harness();
    let txn = settled_uncategorized(h.user_id, h.month_id, Money::from_minor(-2_500));
    let txn_id = txn.id;
    h.transactions.insert(txn);

    h.triage
        .triage(
            TriageInput {
                transaction_id: txn_id,
                category_id: Some(CategoryId::generate()),
                comment: None,
                treatment: Treatment::PayDirectly,
            },
            Utc::now(),
        )
        .await
        .expect("first triage ok");

    // Second triage on the now-categorized row is rejected (it has left the inbox).
    let err = h
        .triage
        .triage(
            TriageInput {
                transaction_id: txn_id,
                category_id: Some(CategoryId::generate()),
                comment: None,
                treatment: Treatment::PayDirectly,
            },
            Utc::now(),
        )
        .await
        .expect_err("a second triage must be rejected");
    assert!(matches!(err, DomainError::IllegalState(_)));
}

#[tokio::test]
async fn spread_over_months_requires_a_buffer_fund() {
    // A surplus fund (compulsory_repayment=false) cannot back a buffer-financed
    // spread — the wrong-kind fund is rejected and nothing is written.
    let h = harness();
    let surplus = surplus_fund(h.user_id, Money::from_major(1_000));
    let fund_id = surplus.id;
    h.funds.store.lock().unwrap().funds.push(surplus);

    let txn = settled_uncategorized(h.user_id, h.month_id, Money::from_major(-500));
    let txn_id = txn.id;
    h.transactions.insert(txn);

    let err = h
        .triage
        .triage(
            TriageInput {
                transaction_id: txn_id,
                category_id: Some(CategoryId::generate()),
                comment: None,
                treatment: Treatment::SpreadOverMonths { fund_id, months: 4 },
            },
            Utc::now(),
        )
        .await
        .expect_err("a surplus fund cannot back a spread");
    assert!(matches!(err, DomainError::Invariant(_)));

    // Nothing written: no obligation, the row is untouched (still uncategorized).
    assert!(h.funds.obligations().is_empty());
    let after = h.transactions.get(txn_id);
    assert!(after.category_id.is_none(), "row not mutated on rejection");
}

// ---------------------------------------------------------------------------
// 6. Transfer treatment — SPEC §4.11 D10 / BUDGET-TRANSFER-EXCLUDE-1
// ---------------------------------------------------------------------------

/// A settled, uncategorized bank charge carrying a Plaid `plaid_category` (so the
/// suggestion can be derived) — an inbox row that should auto-suggest Transfer.
fn settled_with_plaid_category(
    user_id: UserId,
    month_id: MonthId,
    amount: Money,
    plaid_category: &str,
) -> Transaction {
    let mut t = settled_uncategorized(user_id, month_id, amount);
    t.plaid_category = Some(plaid_category.to_owned());
    t
}

#[tokio::test]
async fn transfer_treatment_sets_is_transfer_assigns_no_category_and_leaves_inbox() {
    // SPEC §4.11 D10 / property (d): triaging Transfer sets is_transfer=true, assigns
    // NO category, touches NO fund/obligation, and removes the row from the inbox.
    let h = harness();
    // A -$500 checking withdrawal that pays the card — an internal transfer.
    let txn = settled_with_plaid_category(
        h.user_id,
        h.month_id,
        Money::from_minor(-50_000),
        "LOAN_PAYMENTS_CREDIT_CARD_PAYMENT",
    );
    let txn_id = txn.id;
    h.transactions.insert(txn);

    // It is in the inbox BEFORE triage.
    let before = h.triage.pending_inbox(h.user_id).await.expect("inbox ok");
    assert!(
        before.iter().any(|p| p.id == txn_id),
        "the transfer row is in the inbox before triage"
    );
    // And the inbox row carries the auto-suggest (property (c) at the DTO level).
    assert!(
        before
            .iter()
            .find(|p| p.id == txn_id)
            .expect("row present")
            .suggested_transfer,
        "a LOAN_PAYMENTS_CREDIT_CARD_PAYMENT row suggests Transfer"
    );

    let out = h
        .triage
        .triage(
            TriageInput {
                transaction_id: txn_id,
                category_id: None, // Transfer requires NO category
                comment: Some("paid Amex".to_owned()),
                treatment: Treatment::Transfer,
            },
            Utc::now(),
        )
        .await
        .expect("transfer triage ok");
    assert_eq!(out.obligation_id, None, "Transfer creates no obligation");

    let saved = h.transactions.get(txn_id);
    assert!(saved.is_transfer, "is_transfer set to true");
    assert_eq!(saved.category_id, None, "Transfer assigns NO category");
    assert!(!saved.is_fund_draw, "Transfer is not a fund draw");
    assert_eq!(saved.amount, Money::from_minor(-50_000), "amount unchanged");
    assert_eq!(saved.comment.as_deref(), Some("paid Amex"));

    // It LEFT the inbox — via is_transfer=true, not a category.
    let after = h.triage.pending_inbox(h.user_id).await.expect("inbox ok");
    assert!(
        after.iter().all(|p| p.id != txn_id),
        "the triaged transfer no longer appears in the inbox"
    );

    // No fund / obligation touched.
    assert!(h.funds.obligations().is_empty());
    assert!(h.funds.store.lock().unwrap().funds.is_empty());

    // SQL-aggregate mirror (property (b)): the saved row is excluded from the month
    // expense sum by the same predicate the SQL aggregates mirror with
    // `AND is_transfer = false`.
    assert!(
        !counts_in_month_expense_remaining(&saved, &[], &[]),
        "the triaged transfer is excluded from budget math"
    );
    assert!(
        !oracle_counts_with_transfer(&saved, &[]),
        "independent oracle agrees the transfer is excluded"
    );
}

#[tokio::test]
async fn transfer_inflow_leg_is_triaged_and_excluded_not_counted_as_income() {
    // SPEC §4.11 D10 / property (e): the card-side payment CREDIT (a POSITIVE amount)
    // triaged as a Transfer is excluded — never counted as income. is_income() stays
    // false (income_kind is None) and is_transfer excludes it.
    let h = harness();
    let credit = settled_with_plaid_category(
        h.user_id,
        h.month_id,
        Money::from_minor(50_000), // +$500 inbound payment credit
        "TRANSFER_IN",
    );
    let txn_id = credit.id;
    h.transactions.insert(credit);

    h.triage
        .triage(
            TriageInput {
                transaction_id: txn_id,
                category_id: None,
                comment: None,
                treatment: Treatment::Transfer,
            },
            Utc::now(),
        )
        .await
        .expect("inflow transfer triage ok");

    let saved = h.transactions.get(txn_id);
    assert!(saved.is_transfer);
    assert!(!saved.is_income(), "a transfer inflow is not income");
    assert!(
        !counts_in_month_expense_remaining(&saved, &[], &[]),
        "the positive-inflow transfer is excluded (not counted as income)"
    );
}

#[tokio::test]
async fn re_triaging_an_already_transfer_row_is_rejected() {
    // Guard symmetry: a row already flagged is_transfer has left the inbox; a second
    // triage (of any treatment) is rejected, exactly like re-triaging a categorized
    // row — and nothing is mutated.
    let h = harness();
    let mut txn = settled_uncategorized(h.user_id, h.month_id, Money::from_minor(-50_000));
    txn.is_transfer = true; // already a transfer
    let txn_id = txn.id;
    h.transactions.insert(txn);

    let err = h
        .triage
        .triage(
            TriageInput {
                transaction_id: txn_id,
                category_id: Some(CategoryId::generate()),
                comment: None,
                treatment: Treatment::PayDirectly,
            },
            Utc::now(),
        )
        .await
        .expect_err("an already-transfer row cannot be re-triaged");
    assert!(matches!(err, DomainError::IllegalState(_)));

    // Untouched: still a transfer, still no category.
    let after = h.transactions.get(txn_id);
    assert!(after.is_transfer, "still a transfer");
    assert!(after.category_id.is_none(), "row not mutated on rejection");
}

#[tokio::test]
async fn ordinary_plaid_category_does_not_suggest_transfer() {
    // SPEC §4.11 D10 / property (c) negative case: an ordinary Plaid category
    // (GENERAL_MERCHANDISE) does NOT pre-select Transfer in the inbox DTO.
    let h = harness();
    let txn = settled_with_plaid_category(
        h.user_id,
        h.month_id,
        Money::from_minor(-4_250),
        "GENERAL_MERCHANDISE",
    );
    let txn_id = txn.id;
    h.transactions.insert(txn);

    let inbox = h.triage.pending_inbox(h.user_id).await.expect("inbox ok");
    let row = inbox.iter().find(|p| p.id == txn_id).expect("row present");
    assert!(
        !row.suggested_transfer,
        "GENERAL_MERCHANDISE must NOT suggest a transfer"
    );
}

/// Independent oracle including the transfer exclusion (extends [`oracle_counts`]
/// with the `is_transfer` term) — re-derived inline so the assertion does not
/// tautologically green against the build's own predicate.
fn oracle_counts_with_transfer(t: &Transaction, buffer_financed_ids: &[TransactionId]) -> bool {
    if t.is_transfer {
        return false;
    }
    oracle_counts(t, buffer_financed_ids)
}
