//! Independent (adversarial) tests for [`SeaOrmPlaidSyncEngine`] (`SPEC §6`).
//!
//! These were authored separately from the build to attack the invariants the
//! first test pass left under-proven (`ORCH-NEW-PATH-TESTS-1`,
//! `PROC-REGRESSION-TEST-1`). Like the companion suite they are FULLY MOCKED —
//! NO live Plaid call (`SPEC §6`); live creds are a deploy-time step. The
//! difference from `plaid_sync_engine.rs` is in WHAT is asserted:
//!
//!   - the rolling 30-day reconcile catches a *change to an already-stored row*
//!     (amount + pending->settled + category drift), not only a brand-new row
//!     (`SPEC §6`);
//!   - `removed` restores the fixed-category PLACEHOLDER, proven by evaluating
//!     [`fixed_category_spent`] over the (now-empty) settled set before and after
//!     the removal (`BUDGET-SETTLE-ON-MATCH-1`, `BUDGET-NO-DOUBLE-CHARGE-1`);
//!   - a Plaid CREDIT/REFUND (negative Plaid amount) flows through the whole
//!     engine to a positive internal inflow (`BUDGET-PLAID-SIGN-1`), proving the
//!     sign flip end-to-end (the companion suite only covered debits in the
//!     engine);
//!   - the genesis cutover guard bites on the `modified` and `reconcile` paths,
//!     not only on `added` (`BUDGET-CUTOVER-1`);
//!   - dedup holds for a transaction id REPEATED WITHIN a single page, and a
//!     re-run of a `modified` page is idempotent (no duplicate, no double-apply).
//!
//! A `FakeTxnRepo` that actually computes `category_spent_for_month` from its
//! stored rows is what lets the settlement-reversal assertion be end-to-end
//! rather than a comment.

#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]
#![allow(clippy::panic)]

use std::any::Any;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::{NaiveDate, Utc};
use rust_decimal::Decimal;

use budget_domain::account::Account;
use budget_domain::enums::{MonthStatus, TransactionStatus};
use budget_domain::ids::{AccountId, BudgetId, MonthId, PlaidItemId, TransactionId, UserId};
use budget_domain::money::Money;
use budget_domain::month::Month;
use budget_domain::plaid_api::{
    AccessTokenExchange, LinkToken, LinkTokenRequest, PlaidApi, PlaidError, PlaidSyncEngine,
    PlaidSyncPage, PlaidTransaction,
};
use budget_domain::plaid_item::PlaidItem;
use budget_domain::predicates::{FixedSettlement, fixed_category_spent};
use budget_domain::projections::{CategorySpent, MonthNet};
use budget_domain::repositories::{MonthRepository, PlaidItemRepository, TransactionRepository};
use budget_domain::transaction::Transaction;
use budget_domain::uow::{UnitOfWork, UowFuture, UowProvider};
use budget_domain::{CategoryId, RepositoryError};

use budget_infrastructure::SeaOrmPlaidSyncEngine;

// ---------------------------------------------------------------------------
// UoW fakes
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

fn map_poison<T>(_e: std::sync::PoisonError<T>) -> PlaidError {
    PlaidError::Api("test mutex poisoned".to_owned())
}

// ---------------------------------------------------------------------------
// Scripted mock PlaidApi (no network)
// ---------------------------------------------------------------------------

#[derive(Default)]
struct MockPlaidApi {
    pages: Mutex<Vec<PlaidSyncPage>>,
    reconcile_page: Mutex<Option<PlaidSyncPage>>,
}

#[async_trait]
impl PlaidApi for MockPlaidApi {
    async fn create_link_token(
        &self,
        _request: &LinkTokenRequest,
    ) -> Result<LinkToken, PlaidError> {
        Ok(LinkToken("unused".to_owned()))
    }

    async fn exchange_public_token(
        &self,
        _public_token: &str,
    ) -> Result<AccessTokenExchange, PlaidError> {
        Ok(AccessTokenExchange {
            access_token: "unused".to_owned(),
            plaid_item_id: "unused".to_owned(),
        })
    }

    async fn transactions_sync(
        &self,
        _access_token: &str,
        _cursor: Option<&str>,
    ) -> Result<PlaidSyncPage, PlaidError> {
        let mut pages = self.pages.lock().map_err(map_poison)?;
        if pages.is_empty() {
            let recon = self.reconcile_page.lock().map_err(map_poison)?.take();
            return Ok(recon.unwrap_or_else(empty_page));
        }
        Ok(pages.remove(0))
    }
}

fn empty_page() -> PlaidSyncPage {
    PlaidSyncPage {
        added: vec![],
        modified: vec![],
        removed: vec![],
        accounts: vec![],
        next_cursor: "end".to_owned(),
        has_more: false,
    }
}

// ---------------------------------------------------------------------------
// In-memory repository fakes (the txn repo COMPUTES category_spent_for_month so
// the settlement-reversal assertion can be end-to-end)
// ---------------------------------------------------------------------------

#[derive(Default)]
struct FakeTxnRepo {
    rows: Mutex<Vec<Transaction>>,
}

#[async_trait]
impl TransactionRepository for FakeTxnRepo {
    async fn find_by_id(&self, id: TransactionId) -> Result<Option<Transaction>, RepositoryError> {
        Ok(self
            .rows
            .lock()
            .map_err(poisoned)?
            .iter()
            .find(|t| t.id == id)
            .cloned())
    }

    async fn list_for_month(&self, month_id: MonthId) -> Result<Vec<Transaction>, RepositoryError> {
        Ok(self
            .rows
            .lock()
            .map_err(poisoned)?
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
        Ok(self
            .rows
            .lock()
            .map_err(poisoned)?
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
            .rows
            .lock()
            .map_err(poisoned)?
            .iter()
            .find(|t| t.month_id == month_id && t.is_rollover)
            .cloned())
    }

    async fn find_by_plaid_transaction_id(
        &self,
        plaid_transaction_id: &str,
    ) -> Result<Option<Transaction>, RepositoryError> {
        Ok(self
            .rows
            .lock()
            .map_err(poisoned)?
            .iter()
            .find(|t| t.plaid_transaction_id.as_deref() == Some(plaid_transaction_id))
            .cloned())
    }

    async fn list_expected_for_month(
        &self,
        month_id: MonthId,
    ) -> Result<Vec<Transaction>, RepositoryError> {
        Ok(self
            .rows
            .lock()
            .map_err(poisoned)?
            .iter()
            .filter(|t| t.month_id == month_id && t.status == TransactionStatus::Expected)
            .cloned()
            .collect())
    }

    async fn find_expected_matched_to(
        &self,
        real_transaction_id: TransactionId,
    ) -> Result<Option<Transaction>, RepositoryError> {
        Ok(self
            .rows
            .lock()
            .map_err(poisoned)?
            .iter()
            .find(|t| t.matched_transaction_id == Some(real_transaction_id))
            .cloned())
    }

    /// Computes the signed budget-counting (settled + expected; pending excluded)
    /// sum per category, mirroring `BUDGET-STATUS-DRIVES-INCLUSION-1`. A matched
    /// expected placeholder is excluded (it links to a real txn that counts
    /// instead, `BUDGET-SETTLE-ON-MATCH-1`). Used by the settlement-reversal test
    /// to feed `fixed_category_spent`.
    async fn category_spent_for_month(
        &self,
        month_id: MonthId,
    ) -> Result<Vec<CategorySpent>, RepositoryError> {
        let rows = self.rows.lock().map_err(poisoned)?;
        let mut by_cat: HashMap<CategoryId, Money> = HashMap::new();
        for t in rows.iter() {
            let counts = matches!(
                t.status,
                TransactionStatus::Settled | TransactionStatus::Expected
            ) && !t.is_matched_placeholder();
            if !counts || t.month_id != month_id {
                continue;
            }
            if let Some(cat) = t.category_id {
                let entry = by_cat.entry(cat).or_insert(Money::ZERO);
                *entry += t.amount;
            }
        }
        Ok(by_cat
            .into_iter()
            .map(|(category_id, spent)| CategorySpent { category_id, spent })
            .collect())
    }

    async fn month_net(&self, month_id: MonthId) -> Result<MonthNet, RepositoryError> {
        let rows = self.rows.lock().map_err(poisoned)?;
        let net = rows
            .iter()
            .filter(|t| {
                t.month_id == month_id
                    && matches!(
                        t.status,
                        TransactionStatus::Settled | TransactionStatus::Expected
                    )
                    && !t.is_matched_placeholder()
            })
            .fold(Money::ZERO, |acc, t| acc + t.amount);
        Ok(MonthNet { month_id, net })
    }

    async fn save(
        &self,
        transaction: &Transaction,
        _uow: Option<&dyn UnitOfWork>,
    ) -> Result<(), RepositoryError> {
        let mut rows = self.rows.lock().map_err(poisoned)?;
        if let Some(pos) = rows.iter().position(|t| {
            t.id == transaction.id
                || (transaction.plaid_transaction_id.is_some()
                    && t.plaid_transaction_id == transaction.plaid_transaction_id)
        }) {
            rows[pos] = transaction.clone();
        } else {
            rows.push(transaction.clone());
        }
        Ok(())
    }

    async fn delete(
        &self,
        id: TransactionId,
        _uow: Option<&dyn UnitOfWork>,
    ) -> Result<(), RepositoryError> {
        self.rows.lock().map_err(poisoned)?.retain(|t| t.id != id);
        Ok(())
    }
}

struct FakeMonthRepo {
    months: Vec<Month>,
}

#[async_trait]
impl MonthRepository for FakeMonthRepo {
    async fn find_by_id(&self, id: MonthId) -> Result<Option<Month>, RepositoryError> {
        Ok(self.months.iter().find(|m| m.id == id).cloned())
    }

    async fn find_by_year_month(
        &self,
        user_id: UserId,
        year: i32,
        month: i32,
    ) -> Result<Option<Month>, RepositoryError> {
        Ok(self
            .months
            .iter()
            .find(|m| m.user_id == user_id && m.year == year && m.month == month)
            .cloned())
    }

    async fn find_latest(&self, _user_id: UserId) -> Result<Option<Month>, RepositoryError> {
        Ok(self.months.last().cloned())
    }

    async fn list_for_user(&self, user_id: UserId) -> Result<Vec<Month>, RepositoryError> {
        Ok(self
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
        Ok(month.clone())
    }

    async fn save(
        &self,
        _month: &Month,
        _uow: Option<&dyn UnitOfWork>,
    ) -> Result<(), RepositoryError> {
        Ok(())
    }
}

#[derive(Default)]
struct FakePlaidItemRepo {
    cursor: Mutex<Option<String>>,
    accounts: Mutex<HashMap<String, Account>>,
}

#[async_trait]
impl PlaidItemRepository for FakePlaidItemRepo {
    async fn find_by_id(&self, _id: PlaidItemId) -> Result<Option<PlaidItem>, RepositoryError> {
        Ok(None)
    }

    async fn list_for_user(&self, _user_id: UserId) -> Result<Vec<PlaidItem>, RepositoryError> {
        Ok(vec![])
    }

    async fn get_sync_cursor(&self, _id: PlaidItemId) -> Result<Option<String>, RepositoryError> {
        Ok(self.cursor.lock().map_err(poisoned)?.clone())
    }

    async fn update_sync_cursor(
        &self,
        _id: PlaidItemId,
        cursor: &str,
        _uow: Option<&dyn UnitOfWork>,
    ) -> Result<(), RepositoryError> {
        *self.cursor.lock().map_err(poisoned)? = Some(cursor.to_owned());
        Ok(())
    }

    async fn save(
        &self,
        _item: &PlaidItem,
        _uow: Option<&dyn UnitOfWork>,
    ) -> Result<(), RepositoryError> {
        Ok(())
    }

    async fn list_accounts(&self, _user_id: UserId) -> Result<Vec<Account>, RepositoryError> {
        Ok(vec![])
    }

    async fn find_account(&self, _id: AccountId) -> Result<Option<Account>, RepositoryError> {
        Ok(None)
    }

    async fn find_account_by_plaid_id(
        &self,
        plaid_account_id: &str,
    ) -> Result<Option<Account>, RepositoryError> {
        Ok(self
            .accounts
            .lock()
            .map_err(poisoned)?
            .get(plaid_account_id)
            .cloned())
    }

    async fn save_account(
        &self,
        _account: &Account,
        _uow: Option<&dyn UnitOfWork>,
    ) -> Result<(), RepositoryError> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Builders
// ---------------------------------------------------------------------------

fn date(y: i32, m: u32, d: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, d).unwrap()
}

fn month_row(user_id: UserId, year: i32, month: i32) -> Month {
    Month {
        id: MonthId::generate(),
        user_id,
        budget_id: BudgetId::generate(),
        year,
        month,
        status: MonthStatus::Open,
        opened_at: Utc::now(),
        closed_at: None,
    }
}

fn plaid_txn(id: &str, amount: Decimal, d: NaiveDate, pending: bool) -> PlaidTransaction {
    PlaidTransaction {
        transaction_id: id.to_owned(),
        account_id: "acct-1".to_owned(),
        amount,
        date: d,
        name: format!("merchant-{id}"),
        pending,
        pending_transaction_id: None,
    }
}

fn page(
    added: Vec<PlaidTransaction>,
    modified: Vec<PlaidTransaction>,
    removed: Vec<String>,
    cursor: &str,
    has_more: bool,
) -> PlaidSyncPage {
    PlaidSyncPage {
        added,
        modified,
        removed,
        accounts: vec![],
        next_cursor: cursor.to_owned(),
        has_more,
    }
}

struct Harness {
    engine: SeaOrmPlaidSyncEngine,
    txns: Arc<FakeTxnRepo>,
    user_id: UserId,
    month_id: MonthId,
}

/// Builds an engine over a fresh store, returning the (single) June month id so
/// tests can read category-spent for it.
fn harness_with(pages: Vec<PlaidSyncPage>, reconcile: Option<PlaidSyncPage>) -> Harness {
    let user_id = UserId::generate();
    let may = month_row(user_id, 2026, 5);
    let june = month_row(user_id, 2026, 6);
    let month_id = june.id;
    let months = FakeMonthRepo {
        months: vec![may, june],
    };
    let plaid = Arc::new(MockPlaidApi {
        pages: Mutex::new(pages),
        reconcile_page: Mutex::new(reconcile),
    });
    let txns = Arc::new(FakeTxnRepo::default());
    let items = Arc::new(FakePlaidItemRepo::default());
    let uow = Arc::new(FakeUowProvider);
    let engine = SeaOrmPlaidSyncEngine::new(
        plaid,
        Arc::<FakeTxnRepo>::clone(&txns),
        Arc::new(months),
        items,
        uow,
    );
    Harness {
        engine,
        txns,
        user_id,
        month_id,
    }
}

/// Rebuild a fresh engine over an EXISTING store, so a second `sync_item` call
/// reads the rows the first one wrote (simulating a later sync). Cursor state is
/// fresh (a fresh `FakePlaidItemRepo`); the relevant invariant under test is row
/// dedup/idempotency, not cursor persistence (covered in the companion suite).
fn engine_over(
    txns: &Arc<FakeTxnRepo>,
    user_id: UserId,
    pages: Vec<PlaidSyncPage>,
    reconcile: Option<PlaidSyncPage>,
) -> SeaOrmPlaidSyncEngine {
    let months = FakeMonthRepo {
        months: vec![month_row(user_id, 2026, 5), month_row(user_id, 2026, 6)],
    };
    let plaid = Arc::new(MockPlaidApi {
        pages: Mutex::new(pages),
        reconcile_page: Mutex::new(reconcile),
    });
    SeaOrmPlaidSyncEngine::new(
        plaid,
        Arc::<FakeTxnRepo>::clone(txns),
        Arc::new(months),
        Arc::new(FakePlaidItemRepo::default()),
        Arc::new(FakeUowProvider),
    )
}

const ITEM: fn() -> PlaidItemId = PlaidItemId::generate;
const TSD: fn() -> NaiveDate = || date(2026, 6, 1);
const TODAY: fn() -> NaiveDate = || date(2026, 6, 8);

// ===========================================================================
// (e) SIGN — credit/refund through the WHOLE engine (companion suite only
// covered debits at the engine level; credits only in mapper unit tests).
// ===========================================================================

#[tokio::test]
async fn plaid_credit_refund_becomes_positive_inflow_through_the_engine() {
    // Plaid reports a $40 refund as a NEGATIVE amount (-40.00). After the single
    // mapper flip (BUDGET-PLAID-SIGN-1) it must land as a POSITIVE internal inflow.
    let refund = plaid_txn("refund-1", Decimal::new(-4000, 2), date(2026, 6, 4), false);
    let debit = plaid_txn("debit-1", Decimal::new(4000, 2), date(2026, 6, 4), false);
    let h = harness_with(
        vec![page(vec![refund, debit], vec![], vec![], "c1", false)],
        None,
    );
    h.engine
        .sync_item(ITEM(), h.user_id, "tok", TSD(), TODAY())
        .await
        .unwrap();

    let rows = h.txns.rows.lock().unwrap();
    let r = rows
        .iter()
        .find(|t| t.plaid_transaction_id.as_deref() == Some("refund-1"))
        .unwrap();
    assert!(
        r.amount.is_positive(),
        "a Plaid credit/refund (negative Plaid amount) must become a positive inflow"
    );
    assert_eq!(r.amount.as_decimal(), Decimal::new(4000, 2));
    // And the matching debit on the same day is the opposite sign — proving the
    // flip is per-row, not a blanket negate.
    let d = rows
        .iter()
        .find(|t| t.plaid_transaction_id.as_deref() == Some("debit-1"))
        .unwrap();
    assert_eq!(d.amount.as_decimal(), Decimal::new(-4000, 2));
}

// ===========================================================================
// (c) ROLLING 30-DAY RECONCILE — catches a CHANGE to an ALREADY-STORED row.
// The companion suite only proved a NEW recent row gets reconciled in; this
// proves amount/pending DRIFT on an existing row is corrected.
// ===========================================================================

#[tokio::test]
async fn reconcile_corrects_amount_and_pending_drift_on_an_existing_row() {
    // First sync: a pending $30 charge arrives via `added`.
    let h = harness_with(
        vec![page(
            vec![plaid_txn(
                "drift",
                Decimal::new(3000, 2),
                date(2026, 6, 5),
                true,
            )],
            vec![],
            vec![],
            "c1",
            false,
        )],
        None,
    );
    h.engine
        .sync_item(ITEM(), h.user_id, "tok", TSD(), TODAY())
        .await
        .unwrap();
    {
        let rows = h.txns.rows.lock().unwrap();
        let t = rows
            .iter()
            .find(|t| t.plaid_transaction_id.as_deref() == Some("drift"))
            .unwrap();
        assert_eq!(t.status, TransactionStatus::Pending);
        assert_eq!(t.amount.as_decimal(), Decimal::new(-3000, 2));
    }

    // Second sync: the incremental stream is EMPTY (drift was missed), but the
    // rolling reconcile re-pull reports the SAME id now SETTLED at a corrected
    // $31.50. The reconcile must upsert it in place (same row, new state).
    let corrected = plaid_txn("drift", Decimal::new(3150, 2), date(2026, 6, 5), false);
    // The cursor loop must see an explicit EMPTY page first (so it advances and
    // exits); the SEPARATE reconcile re-pull then returns the corrected row. If we
    // left the cursor list empty the loop would drain the reconcile page itself.
    let engine2 = engine_over(
        &h.txns,
        h.user_id,
        vec![page(vec![], vec![], vec![], "c2", false)],
        Some(page(vec![corrected], vec![], vec![], "c3", false)),
    );
    let summary = engine2
        .sync_item(ITEM(), h.user_id, "tok", TSD(), TODAY())
        .await
        .unwrap();
    assert_eq!(summary.reconciled, 1);

    let rows = h.txns.rows.lock().unwrap();
    let updated = rows
        .iter()
        .find(|t| t.plaid_transaction_id.as_deref() == Some("drift"))
        .unwrap();
    assert_eq!(
        updated.status,
        TransactionStatus::Settled,
        "reconcile must catch the pending->settled drift"
    );
    assert_eq!(
        updated.amount.as_decimal(),
        Decimal::new(-3150, 2),
        "reconcile must catch the amount drift (still sign-flipped)"
    );
    assert_eq!(
        rows.len(),
        1,
        "reconcile upserts in place — no duplicate row"
    );
}

#[tokio::test]
async fn reconcile_skips_a_row_dated_after_today_upper_bound() {
    // The reconcile window is [lower_bound, today]. A row dated in the FUTURE
    // (after `today`) must be excluded by the upper bound, even though it is after
    // tracking_start. (Defends the `t.date <= today` clamp.)
    let future = plaid_txn("future", Decimal::new(5000, 2), date(2026, 6, 20), false);
    let h = harness_with(
        vec![page(vec![], vec![], vec![], "c1", false)],
        Some(page(vec![future], vec![], vec![], "c2", false)),
    );
    let summary = h
        .engine
        .sync_item(ITEM(), h.user_id, "tok", TSD(), TODAY())
        .await
        .unwrap();
    assert_eq!(
        summary.reconciled, 0,
        "a future-dated row is past the upper bound"
    );
    assert!(
        h.txns.rows.lock().unwrap().is_empty(),
        "the future-dated row must not be ingested by the reconcile pass"
    );
}

// ===========================================================================
// (a) REMOVED restores the PLACEHOLDER — proven via fixed_category_spent over
// the stored settled set, not just by asserting deletion.
// ===========================================================================

#[tokio::test]
async fn removed_settled_row_restores_fixed_category_placeholder() {
    // Model a fixed "rent" category: budgeted placeholder -$2,000. A real settled
    // rent charge ($2,015) arrives via Plaid and is (in a later step) assigned to
    // the category. We simulate that assignment by stamping category_id on the
    // stored row. While a settled row exists, the category is Settled and spent =
    // the real sum. After `removed`, no settled rows remain -> Unsettled -> the
    // placeholder stands back in (BUDGET-SETTLE-ON-MATCH-1 / BUDGET-NO-DOUBLE-CHARGE-1).
    let rent_cat = CategoryId::generate();
    let placeholder = Money::from_minor(-200_000); // -$2,000

    let h = harness_with(
        vec![page(
            vec![plaid_txn(
                "rent",
                Decimal::new(201_500, 2),
                date(2026, 6, 2),
                false,
            )],
            vec![],
            vec![],
            "c1",
            false,
        )],
        None,
    );
    h.engine
        .sync_item(ITEM(), h.user_id, "tok", TSD(), TODAY())
        .await
        .unwrap();

    // Simulate the §7 user assignment of the Plaid row to the rent category.
    {
        let mut rows = h.txns.rows.lock().unwrap();
        let row = rows
            .iter_mut()
            .find(|t| t.plaid_transaction_id.as_deref() == Some("rent"))
            .unwrap();
        row.category_id = Some(rent_cat);
    }

    // Settlement state BEFORE removal: a settled real row exists -> Settled.
    let spent_before = {
        let spent_rows = h.txns.category_spent_for_month(h.month_id).await.unwrap();
        let sum = spent_rows
            .iter()
            .find(|c| c.category_id == rent_cat)
            .map_or(Money::ZERO, |c| c.spent);
        fixed_category_spent(FixedSettlement::Settled, placeholder, sum)
    };
    assert_eq!(
        spent_before,
        Money::from_minor(-201_500),
        "while a settled row exists, spent is the REAL sum (placeholder replaced)"
    );

    // Plaid removes/reverses the charge.
    let engine2 = engine_over(
        &h.txns,
        h.user_id,
        vec![page(vec![], vec![], vec!["rent".to_owned()], "c2", false)],
        None,
    );
    let summary = engine2
        .sync_item(ITEM(), h.user_id, "tok", TSD(), TODAY())
        .await
        .unwrap();
    assert_eq!(summary.removed, 1);

    // AFTER removal: no settled rows remain -> Unsettled -> placeholder restored.
    let spent_after = {
        let spent_rows = h.txns.category_spent_for_month(h.month_id).await.unwrap();
        let sum = spent_rows
            .iter()
            .find(|c| c.category_id == rent_cat)
            .map_or(Money::ZERO, |c| c.spent);
        // The category is now Unsettled (the settled sum is zero / category gone).
        assert_eq!(sum, Money::ZERO, "no settled rows remain after removal");
        fixed_category_spent(FixedSettlement::Unsettled, placeholder, sum)
    };
    assert_eq!(
        spent_after, placeholder,
        "removing the only settled row restores the budgeted placeholder \
         (BUDGET-SETTLE-ON-MATCH-1)"
    );
}

// ===========================================================================
// (d) GENESIS CUTOVER on the MODIFIED + RECONCILE paths (companion suite only
// proved it on `added`).
// ===========================================================================

#[tokio::test]
async fn cutover_guard_skips_a_pre_genesis_row_on_the_modified_path() {
    // A `modified` row dated before tracking_start must be dropped — it must NOT
    // create (or resurrect) a pre-genesis row. tracking_start = 2026-06-01.
    let pre = plaid_txn("pre-mod", Decimal::new(7777, 2), date(2026, 5, 20), false);
    let h = harness_with(vec![page(vec![], vec![pre], vec![], "c1", false)], None);
    let summary = h
        .engine
        .sync_item(ITEM(), h.user_id, "tok", TSD(), TODAY())
        .await
        .unwrap();
    assert_eq!(summary.modified, 0, "a pre-genesis modified row is dropped");
    assert!(
        h.txns.rows.lock().unwrap().is_empty(),
        "a pre-genesis modified row must never be written (BUDGET-CUTOVER-1)"
    );
}

#[tokio::test]
async fn cutover_guard_drops_pre_genesis_row_in_the_reconcile_window() {
    // Mixed reconcile batch: one pre-genesis (2026-05-15) and one in-window
    // (2026-06-07). Only the on-or-after row is ingested; the pre-date one is
    // dropped (the double-count guard, BUDGET-CUTOVER-1).
    let recon = page(
        vec![
            plaid_txn("pre-recon", Decimal::new(9999, 2), date(2026, 5, 15), false),
            plaid_txn("in-recon", Decimal::new(1234, 2), date(2026, 6, 7), false),
        ],
        vec![],
        vec![],
        "c2",
        false,
    );
    let h = harness_with(vec![page(vec![], vec![], vec![], "c1", false)], Some(recon));
    let summary = h
        .engine
        .sync_item(ITEM(), h.user_id, "tok", TSD(), TODAY())
        .await
        .unwrap();
    assert_eq!(summary.reconciled, 1, "only the in-window row reconciles");

    let rows = h.txns.rows.lock().unwrap();
    assert!(
        rows.iter()
            .all(|t| t.plaid_transaction_id.as_deref() != Some("pre-recon")),
        "the pre-genesis row must never be ingested by reconcile (BUDGET-CUTOVER-1)"
    );
    assert!(
        rows.iter()
            .any(|t| t.plaid_transaction_id.as_deref() == Some("in-recon")),
        "the in-window row is ingested"
    );
}

// ===========================================================================
// (b) DEDUP + IDEMPOTENCY — within a single page, and across a `modified` re-run.
// ===========================================================================

#[tokio::test]
async fn duplicate_id_within_a_single_added_page_is_deduped() {
    // Plaid should not, but the dedup must hold even if a page repeats an id: the
    // second occurrence is deduped by plaid_transaction_id (UNIQUE), not inserted.
    let dup_a = plaid_txn("same", Decimal::new(2000, 2), date(2026, 6, 5), false);
    let dup_b = plaid_txn("same", Decimal::new(2000, 2), date(2026, 6, 5), false);
    let h = harness_with(
        vec![page(vec![dup_a, dup_b], vec![], vec![], "c1", false)],
        None,
    );
    let summary = h
        .engine
        .sync_item(ITEM(), h.user_id, "tok", TSD(), TODAY())
        .await
        .unwrap();
    assert_eq!(summary.added, 1, "the repeated id is inserted exactly once");
    assert_eq!(
        h.txns.rows.lock().unwrap().len(),
        1,
        "no duplicate row from a repeated id within one page"
    );
}

#[tokio::test]
async fn re_running_a_modified_page_is_idempotent() {
    // Seed a settled row, then apply the SAME `modified` page twice. The row must
    // be upserted in place both times — no duplicate, no double-apply.
    let h = harness_with(
        vec![page(
            vec![plaid_txn(
                "m",
                Decimal::new(5000, 2),
                date(2026, 6, 3),
                false,
            )],
            vec![],
            vec![],
            "c1",
            false,
        )],
        None,
    );
    h.engine
        .sync_item(ITEM(), h.user_id, "tok", TSD(), TODAY())
        .await
        .unwrap();

    let modified = plaid_txn("m", Decimal::new(5500, 2), date(2026, 6, 3), false);
    let mk = || vec![page(vec![], vec![modified.clone()], vec![], "c2", false)];
    let engine2 = engine_over(&h.txns, h.user_id, mk(), None);
    engine2
        .sync_item(ITEM(), h.user_id, "tok", TSD(), TODAY())
        .await
        .unwrap();
    let engine3 = engine_over(&h.txns, h.user_id, mk(), None);
    engine3
        .sync_item(ITEM(), h.user_id, "tok", TSD(), TODAY())
        .await
        .unwrap();

    let rows = h.txns.rows.lock().unwrap();
    assert_eq!(rows.len(), 1, "re-running modified never duplicates");
    assert_eq!(
        rows[0].amount.as_decimal(),
        Decimal::new(-5500, 2),
        "the modified amount applied (idempotently)"
    );
}

// ===========================================================================
// LIVE PLAID — gated; runs ONLY with explicit creds (deploy-time step, SPEC §6).
// Never runs in CI; documents the intended live smoke shape.
// ===========================================================================

#[tokio::test]
#[ignore = "live Plaid call; deploy-time only (SPEC §6) — never run in CI"]
async fn live_plaid_sandbox_smoke_placeholder() {
    // Intentionally a no-op placeholder: a real live smoke would build the
    // HttpPlaidApi from sandbox creds and assert a non-error /transactions/sync.
    // Kept #[ignore] so the gate is explicit and discoverable.
}
