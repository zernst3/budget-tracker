//! Integration tests for [`SeaOrmPlaidSyncEngine`] (`SPEC §6`, build step 8).
//!
//! Fully MOCKED Plaid — NO live call (`SPEC §6`). A scripted [`MockPlaidApi`]
//! returns pre-canned `/transactions/sync` pages; in-memory repository fakes
//! stand in for Postgres. The tests prove the load-bearing sync invariants:
//!   - the genesis cutover guard skips pre-`tracking_start_date` rows
//!     (`BUDGET-CUTOVER-1`);
//!   - the Plaid sign is flipped at the mapper boundary (Plaid positive-outflow
//!     -> internal negative-expense, `BUDGET-PLAID-SIGN-1`);
//!   - status follows Plaid `pending` (excluded) vs settled (included),
//!     `SPEC §4.4`;
//!   - re-running sync is idempotent (dedup by `plaid_transaction_id`);
//!   - the pending->settled transition removes the superseded pending row;
//!   - `removed` deletes the row (settlement reverses via the predicate,
//!     `BUDGET-SETTLE-ON-MATCH-1`);
//!   - multi-page cursor sync loops until `has_more = false`.

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
use budget_domain::month::Month;
use budget_domain::plaid_api::{
    AccessTokenExchange, LinkToken, LinkTokenRequest, PlaidApi, PlaidError, PlaidSyncEngine,
    PlaidSyncPage, PlaidTransaction,
};
use budget_domain::plaid_item::PlaidItem;
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

// ---------------------------------------------------------------------------
// Scripted mock PlaidApi (no network)
// ---------------------------------------------------------------------------

#[derive(Default)]
struct MockPlaidApi {
    /// Pages returned for cursor=Some/None sync calls, in order. The reconcile
    /// re-pull (cursor=None at the end) consumes the LAST queued page if present.
    pages: Mutex<Vec<PlaidSyncPage>>,
    /// The page returned for the reconcile re-pull (cursor=None) AFTER the loop.
    reconcile_page: Mutex<Option<PlaidSyncPage>>,
    /// Count of sync calls (to assert multi-page looping).
    sync_calls: Mutex<usize>,
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
        cursor: Option<&str>,
    ) -> Result<PlaidSyncPage, PlaidError> {
        *self.sync_calls.lock().map_err(map_poison)? += 1;
        // A cursor=None call AFTER the loop pages are exhausted is the reconcile
        // re-pull.
        let mut pages = self.pages.lock().map_err(map_poison)?;
        if pages.is_empty() {
            // Reconcile re-pull or empty: return the reconcile page or an empty one.
            let recon = self.reconcile_page.lock().map_err(map_poison)?.take();
            return Ok(recon.unwrap_or_else(empty_page));
        }
        // Cursor-loop page.
        let _ = cursor;
        Ok(pages.remove(0))
    }
}

fn map_poison<T>(_e: std::sync::PoisonError<T>) -> PlaidError {
    PlaidError::Api("test mutex poisoned".to_owned())
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
// In-memory repository fakes
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

    async fn category_spent_for_month(
        &self,
        _month_id: MonthId,
    ) -> Result<Vec<CategorySpent>, RepositoryError> {
        Ok(vec![])
    }

    async fn month_net(&self, month_id: MonthId) -> Result<MonthNet, RepositoryError> {
        Ok(MonthNet {
            month_id,
            net: budget_domain::Money::ZERO,
        })
    }

    async fn save(
        &self,
        transaction: &Transaction,
        _uow: Option<&dyn UnitOfWork>,
    ) -> Result<(), RepositoryError> {
        // Upsert keyed on plaid_transaction_id (the dedup key) OR id.
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

struct Harness {
    engine: SeaOrmPlaidSyncEngine,
    txns: Arc<FakeTxnRepo>,
    user_id: UserId,
}

fn harness_with(pages: Vec<PlaidSyncPage>, reconcile: Option<PlaidSyncPage>) -> Harness {
    let user_id = UserId::generate();
    // Months for May + June 2026 so post-genesis rows resolve.
    let months = FakeMonthRepo {
        months: vec![month_row(user_id, 2026, 5), month_row(user_id, 2026, 6)],
    };
    let plaid = Arc::new(MockPlaidApi {
        pages: Mutex::new(pages),
        reconcile_page: Mutex::new(reconcile),
        sync_calls: Mutex::new(0),
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
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn added_rows_flip_sign_land_uncategorized_and_status_from_pending() {
    // One settled debit (Plaid +25.00 -> internal -25.00, settled) and one
    // pending debit (Plaid +10.00 -> internal -10.00, pending/excluded).
    let page = PlaidSyncPage {
        added: vec![
            plaid_txn("t-settled", Decimal::new(2500, 2), date(2026, 6, 5), false),
            plaid_txn("t-pending", Decimal::new(1000, 2), date(2026, 6, 6), true),
        ],
        modified: vec![],
        removed: vec![],
        accounts: vec![],
        next_cursor: "c1".to_owned(),
        has_more: false,
    };
    let h = harness_with(vec![page], None);
    let summary = h
        .engine
        .sync_item(
            PlaidItemId::generate(),
            h.user_id,
            "tok",
            date(2026, 6, 1),
            date(2026, 6, 8),
        )
        .await
        .unwrap();
    assert_eq!(summary.added, 2);

    let rows = h.txns.rows.lock().unwrap();
    let settled = rows
        .iter()
        .find(|t| t.plaid_transaction_id.as_deref() == Some("t-settled"))
        .unwrap();
    // BUDGET-PLAID-SIGN-1: Plaid +25 -> internal -25 (expense).
    assert_eq!(settled.amount.as_decimal(), Decimal::new(-2500, 2));
    assert_eq!(settled.status, TransactionStatus::Settled);
    assert!(settled.category_id.is_none(), "lands uncategorized");

    let pending = rows
        .iter()
        .find(|t| t.plaid_transaction_id.as_deref() == Some("t-pending"))
        .unwrap();
    assert_eq!(pending.status, TransactionStatus::Pending);
}

#[tokio::test]
async fn cutover_guard_skips_pre_genesis_rows() {
    // tracking_start = 2026-06-01. A row dated 2026-05-31 must be skipped.
    let page = PlaidSyncPage {
        added: vec![
            plaid_txn("pre", Decimal::new(5000, 2), date(2026, 5, 31), false),
            plaid_txn("post", Decimal::new(5000, 2), date(2026, 6, 2), false),
        ],
        modified: vec![],
        removed: vec![],
        accounts: vec![],
        next_cursor: "c1".to_owned(),
        has_more: false,
    };
    let h = harness_with(vec![page], None);
    let summary = h
        .engine
        .sync_item(
            PlaidItemId::generate(),
            h.user_id,
            "tok",
            date(2026, 6, 1),
            date(2026, 6, 8),
        )
        .await
        .unwrap();
    assert_eq!(summary.added, 1, "only the post-genesis row is ingested");
    assert_eq!(summary.skipped_pre_genesis, 1);

    let rows = h.txns.rows.lock().unwrap();
    assert!(
        rows.iter()
            .all(|t| t.plaid_transaction_id.as_deref() != Some("pre")),
        "the pre-genesis row must never be ingested (BUDGET-CUTOVER-1)"
    );
}

#[tokio::test]
async fn re_running_sync_is_idempotent_no_duplicates() {
    let make_page = || PlaidSyncPage {
        added: vec![plaid_txn(
            "dup",
            Decimal::new(2000, 2),
            date(2026, 6, 5),
            false,
        )],
        modified: vec![],
        removed: vec![],
        accounts: vec![],
        next_cursor: "c1".to_owned(),
        has_more: false,
    };
    // First run inserts; second run sees the same id and dedups.
    let h = harness_with(vec![make_page(), make_page()], None);
    let item = PlaidItemId::generate();
    let s1 = h
        .engine
        .sync_item(item, h.user_id, "tok", date(2026, 6, 1), date(2026, 6, 8))
        .await
        .unwrap();
    assert_eq!(s1.added, 1);
    let s2 = h
        .engine
        .sync_item(item, h.user_id, "tok", date(2026, 6, 1), date(2026, 6, 8))
        .await
        .unwrap();
    assert_eq!(s2.added, 0, "the second run dedups by plaid_transaction_id");
    assert_eq!(
        h.txns.rows.lock().unwrap().len(),
        1,
        "no duplicate row created"
    );
}

#[tokio::test]
async fn pending_to_settled_modified_removes_superseded_pending_row() {
    // Page 1: a pending row arrives via `added`.
    let page1 = PlaidSyncPage {
        added: vec![plaid_txn(
            "p-1",
            Decimal::new(3000, 2),
            date(2026, 6, 5),
            true,
        )],
        modified: vec![],
        removed: vec![],
        accounts: vec![],
        next_cursor: "c1".to_owned(),
        has_more: false,
    };
    let h = harness_with(vec![page1], None);
    let item = PlaidItemId::generate();
    h.engine
        .sync_item(item, h.user_id, "tok", date(2026, 6, 1), date(2026, 6, 8))
        .await
        .unwrap();
    assert_eq!(h.txns.rows.lock().unwrap().len(), 1);

    // Page 2: the SETTLED version arrives under a NEW id, linking back to p-1.
    let mut settled = plaid_txn("s-1", Decimal::new(3000, 2), date(2026, 6, 5), false);
    settled.pending_transaction_id = Some("p-1".to_owned());
    let page2 = PlaidSyncPage {
        added: vec![],
        modified: vec![settled],
        removed: vec![],
        accounts: vec![],
        next_cursor: "c2".to_owned(),
        has_more: false,
    };
    // Re-seed the mock with page2 for the second sync_item call.
    let h2_pages = vec![page2];
    // Reuse the same txn repo + month repo via a fresh engine over the same store.
    let plaid = Arc::new(MockPlaidApi {
        pages: Mutex::new(h2_pages),
        reconcile_page: Mutex::new(None),
        sync_calls: Mutex::new(0),
    });
    let months = FakeMonthRepo {
        months: vec![month_row(h.user_id, 2026, 6)],
    };
    let items = Arc::new(FakePlaidItemRepo::default());
    let uow = Arc::new(FakeUowProvider);
    let engine2 = SeaOrmPlaidSyncEngine::new(
        plaid,
        Arc::<FakeTxnRepo>::clone(&h.txns),
        Arc::new(months),
        items,
        uow,
    );
    engine2
        .sync_item(item, h.user_id, "tok", date(2026, 6, 1), date(2026, 6, 8))
        .await
        .unwrap();

    let rows = h.txns.rows.lock().unwrap();
    assert!(
        rows.iter()
            .all(|t| t.plaid_transaction_id.as_deref() != Some("p-1")),
        "the superseded pending row must be removed"
    );
    let s = rows
        .iter()
        .find(|t| t.plaid_transaction_id.as_deref() == Some("s-1"))
        .unwrap();
    assert_eq!(s.status, TransactionStatus::Settled);
    assert_eq!(rows.len(), 1, "exactly one row remains (counted once)");
}

#[tokio::test]
async fn removed_deletes_the_row() {
    // Insert a row, then remove it in a later page.
    let page1 = PlaidSyncPage {
        added: vec![plaid_txn(
            "r-1",
            Decimal::new(1500, 2),
            date(2026, 6, 5),
            false,
        )],
        modified: vec![],
        removed: vec![],
        accounts: vec![],
        next_cursor: "c1".to_owned(),
        has_more: false,
    };
    let h = harness_with(vec![page1], None);
    let item = PlaidItemId::generate();
    h.engine
        .sync_item(item, h.user_id, "tok", date(2026, 6, 1), date(2026, 6, 8))
        .await
        .unwrap();
    assert_eq!(h.txns.rows.lock().unwrap().len(), 1);

    let page2 = PlaidSyncPage {
        added: vec![],
        modified: vec![],
        removed: vec!["r-1".to_owned()],
        accounts: vec![],
        next_cursor: "c2".to_owned(),
        has_more: false,
    };
    let plaid = Arc::new(MockPlaidApi {
        pages: Mutex::new(vec![page2]),
        reconcile_page: Mutex::new(None),
        sync_calls: Mutex::new(0),
    });
    let months = FakeMonthRepo {
        months: vec![month_row(h.user_id, 2026, 6)],
    };
    let engine2 = SeaOrmPlaidSyncEngine::new(
        plaid,
        Arc::<FakeTxnRepo>::clone(&h.txns),
        Arc::new(months),
        Arc::new(FakePlaidItemRepo::default()),
        Arc::new(FakeUowProvider),
    );
    let summary = engine2
        .sync_item(item, h.user_id, "tok", date(2026, 6, 1), date(2026, 6, 8))
        .await
        .unwrap();
    assert_eq!(summary.removed, 1);
    assert!(
        h.txns.rows.lock().unwrap().is_empty(),
        "removed row deleted; predicate-based settlement reverses automatically"
    );
}

#[tokio::test]
async fn multi_page_cursor_loop_consumes_all_pages() {
    let page1 = PlaidSyncPage {
        added: vec![plaid_txn(
            "a",
            Decimal::new(1000, 2),
            date(2026, 6, 2),
            false,
        )],
        modified: vec![],
        removed: vec![],
        accounts: vec![],
        next_cursor: "c1".to_owned(),
        has_more: true,
    };
    let page2 = PlaidSyncPage {
        added: vec![plaid_txn(
            "b",
            Decimal::new(2000, 2),
            date(2026, 6, 3),
            false,
        )],
        modified: vec![],
        removed: vec![],
        accounts: vec![],
        next_cursor: "c2".to_owned(),
        has_more: false,
    };
    let h = harness_with(vec![page1, page2], None);
    let summary = h
        .engine
        .sync_item(
            PlaidItemId::generate(),
            h.user_id,
            "tok",
            date(2026, 6, 1),
            date(2026, 6, 8),
        )
        .await
        .unwrap();
    assert_eq!(
        summary.added, 2,
        "both pages applied (loop until has_more=false)"
    );
}

#[tokio::test]
async fn reconcile_window_clamps_to_genesis_and_skips_old_rows() {
    // The reconcile re-pull returns a pre-genesis row and an in-window row; only
    // the in-window post-genesis row is re-applied.
    let cursor_page = PlaidSyncPage {
        added: vec![],
        modified: vec![],
        removed: vec![],
        accounts: vec![],
        next_cursor: "c1".to_owned(),
        has_more: false,
    };
    let reconcile = PlaidSyncPage {
        added: vec![
            // pre-genesis (skipped by the clamp)
            plaid_txn("old", Decimal::new(9999, 2), date(2026, 5, 1), false),
            // in-window post-genesis
            plaid_txn("recent", Decimal::new(4242, 2), date(2026, 6, 7), false),
        ],
        modified: vec![],
        removed: vec![],
        accounts: vec![],
        next_cursor: "c2".to_owned(),
        has_more: false,
    };
    let h = harness_with(vec![cursor_page], Some(reconcile));
    let summary = h
        .engine
        .sync_item(
            PlaidItemId::generate(),
            h.user_id,
            "tok",
            date(2026, 6, 1),
            date(2026, 6, 8),
        )
        .await
        .unwrap();
    // Only the in-window row counts toward reconciled.
    assert_eq!(summary.reconciled, 1);
    let rows = h.txns.rows.lock().unwrap();
    assert!(
        rows.iter()
            .any(|t| t.plaid_transaction_id.as_deref() == Some("recent")),
        "the in-window row was reconciled in"
    );
    assert!(
        rows.iter()
            .all(|t| t.plaid_transaction_id.as_deref() != Some("old")),
        "the pre-genesis row is excluded by the reconcile clamp (BUDGET-CUTOVER-1)"
    );
}
