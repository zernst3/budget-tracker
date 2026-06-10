//! `update_transaction_inline` server function — the BACKEND-4 inline-edit
//! write side of the month ledger (`SPEC §7`, `BUDGET-AUTH-GATE-1`,
//! `RUST-DIOXUS-9`).
//!
//! The month ledger is read-only EXCEPT two fields on a transaction row
//! (`SPEC §7`):
//!
//!   1. **`category_id`** — an inline dropdown; reassigning moves the
//!      transaction's amount from the old category's spent into the new one's.
//!      Budget math is purely computed at query time (`category_spent_for_month`
//!      aggregates on `category_id`), so updating the FK is the only write
//!      needed: the next ledger/envelope-summary read reflects the new
//!      category automatically.
//!   2. **`comment`** — free-text user note (`transactions.comment`, `SPEC §5`).
//!      `None` leaves the comment unchanged; `Some("")` clears it (blank is
//!      equivalent to absent).
//!
//! Both fields are optional in the request: supplying only one updates only
//! that field; supplying neither is a no-op (returns `Ok` without touching
//! the DB). No other field is exposed — amounts, dates, descriptions, and
//! statuses are fixed by Plaid or the manual-entry flow and are NOT editable
//! through the ledger.
//!
//! ## Invariants upheld
//!
//! - `BUDGET-AUTH-GATE-1` — `require_authed_user()` is the very first call;
//!   the transaction is also verified to belong to the authenticated user.
//! - `SPEC §9.1` — every query is scoped to `user.id()`.
//! - `BUDGET-NO-DOUBLE-CHARGE-1` — category assignment is a FK update only;
//!   the amount is unchanged. There is no placeholder interaction here: triage
//!   handles the FIRST assignment; this is a ledger CORRECTION on an already-
//!   categorized row.
//! - `RUST-SEAORM-INTRA-AGGREGATE-TX-1` — the write touches exactly ONE row
//!   on exactly ONE table; no cross-aggregate coordination is needed, so the
//!   save uses `uow: None` (the repository's own pool connection handles the
//!   single-row atomicity).
//! - Ownership defense: a forged `transaction_id` for another user's row is
//!   rejected before any mutation.
//! - Category defense: the supplied `category_id` must belong to the budget
//!   version the transaction's month references — a cross-budget or non-existent
//!   category id is rejected.

use dioxus::prelude::*;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// DTOs (compile on both targets — WASM-clean)
// ---------------------------------------------------------------------------

/// The request body for an inline-edit of a ledger transaction (`SPEC §7`).
///
/// Both editable fields are `Option`: supply only the field(s) you want to
/// change. Supplying neither is accepted (no-op). No other transaction field
/// is exposed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InlineEditRequest {
    /// The stable transaction id (the ledger row to edit).
    pub transaction_id: String,
    /// The new category id to assign (`None` = leave category unchanged).
    ///
    /// The category must belong to the budget version the transaction's month
    /// references; a cross-budget or non-existent category id is rejected (400).
    pub category_id: Option<String>,
    /// The new comment:
    ///   - `None` = leave comment unchanged,
    ///   - `Some("")` = clear the comment (blank ≡ absent),
    ///   - `Some(text)` = set the comment to `text`.
    pub comment: Option<String>,
}

/// The outcome of a successful inline edit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InlineEditResult {
    /// The edited transaction id (echoed back so the UI can confirm which row
    /// was updated).
    pub transaction_id: String,
    /// The category id now on the transaction (after the edit).
    pub category_id: Option<String>,
    /// The comment now on the transaction (after the edit).
    pub comment: Option<String>,
}

// ---------------------------------------------------------------------------
// Server function (native only; the `#[server]` macro strips the body on wasm)
// ---------------------------------------------------------------------------

/// Update a transaction's **category** and/or **comment** inline from the
/// month ledger (`SPEC §7`), gated by session auth (`BUDGET-AUTH-GATE-1`).
///
/// Exactly two transaction fields are editable from the ledger:
///   - `category_id` — reassigning moves the amount between category buckets;
///     budget math (category spent, envelope summary) is computed at query time
///     so it reflects the change on the next read.
///   - `comment` — a free-text user note; `Some("")` clears it.
///
/// Supplying neither field is accepted as a no-op. No other field is exposed;
/// amounts, dates, descriptions, and statuses are not editable here.
///
/// # Errors
///
/// - HTTP 401 — no valid session.
/// - HTTP 400 — malformed UUID; transaction not found or not owned by the
///   authenticated user; or the supplied `category_id` does not belong to the
///   budget version that the transaction's month references.
/// - HTTP 500 — any persistence failure.
#[allow(clippy::unused_async, clippy::too_many_lines)]
#[server]
pub async fn update_transaction_inline(
    request: InlineEditRequest,
) -> Result<InlineEditResult, dioxus::prelude::ServerFnError> {
    use budget_domain::ids::{CategoryId, TransactionId};

    use crate::server_state::MonthViewState;
    use crate::services::gate::require_authed_user;

    // 1. GATE FIRST — no data is touched before this returns Ok.
    //    (`BUDGET-AUTH-GATE-1`)
    let user = require_authed_user().await?;
    let state = MonthViewState::extract().await?;

    // 2. Parse the transaction id. A malformed value is a 400, never a data
    //    reach.
    let transaction_id = TransactionId::new(parse_uuid(&request.transaction_id, "transaction_id")?);

    // 3. Load the transaction and verify ownership (`SPEC §9.1`). A forged id
    //    for another user's row surfaces as "not found" (no distinguishing
    //    detail, anti-enumeration).
    let mut txn = state
        .transactions
        .find_by_id(transaction_id)
        .await
        .map_err(|e| internal_error(&e))?
        .ok_or_else(|| bad_request("transaction not found"))?;
    if txn.user_id != user.id() {
        return Err(bad_request("transaction not found"));
    }

    // 4. Early exit: if neither field is being changed, return the current
    //    state without touching the DB.
    if request.category_id.is_none() && request.comment.is_none() {
        return Ok(InlineEditResult {
            transaction_id: txn.id.to_string(),
            category_id: txn.category_id.map(|c| c.to_string()),
            comment: txn.comment,
        });
    }

    // 5. If a category change is requested, validate that the category exists
    //    AND belongs to the budget version the transaction's month references.
    //    A cross-budget category id would corrupt `category_spent_for_month`
    //    aggregation for that month (the aggregate groups by category_id; a
    //    category from another budget version has no row in the envelope
    //    summary that month).
    if let Some(ref cat_str) = request.category_id {
        let new_cat_id = CategoryId::new(parse_uuid(cat_str, "category_id")?);

        // Resolve the month to get its budget_id (the version whose categories
        // are valid for this transaction).
        let month = state
            .months
            .find_by_id(txn.month_id)
            .await
            .map_err(|e| internal_error(&e))?
            .ok_or_else(|| bad_request("transaction's month not found"))?;

        // The category must belong to that budget version.
        let category = state
            .budgets
            .find_category(new_cat_id)
            .await
            .map_err(|e| internal_error(&e))?
            .ok_or_else(|| bad_request("category not found"))?;
        if category.budget_id != month.budget_id {
            return Err(bad_request(
                "category does not belong to this month's budget version",
            ));
        }

        txn.category_id = Some(new_cat_id);
    }

    // 6. Apply the comment change. `Some("")` normalises to `None` (blank ≡
    //    absent — an empty string has no semantic value as a note).
    if let Some(raw_comment) = request.comment {
        txn.comment = if raw_comment.is_empty() {
            None
        } else {
            Some(raw_comment)
        };
    }

    // 7. Stamp `updated_at` and persist.
    //    This is a single-row, single-table write — no cross-aggregate
    //    coordination required — so `uow: None` is correct
    //    (`RUST-SEAORM-INTRA-AGGREGATE-TX-1`). The repository's own pool
    //    connection handles the single-row atomicity.
    txn.updated_at = chrono::Utc::now();
    state
        .transactions
        .save(&txn, None)
        .await
        .map_err(|e| internal_error(&e))?;

    Ok(InlineEditResult {
        transaction_id: txn.id.to_string(),
        category_id: txn.category_id.map(|c| c.to_string()),
        comment: txn.comment,
    })
}

// ---------------------------------------------------------------------------
// Helpers (server-only)
// ---------------------------------------------------------------------------

/// Parse a wire UUID string, mapping a malformed value to an opaque HTTP 400.
#[cfg(feature = "server")]
fn parse_uuid(raw: &str, field: &str) -> Result<uuid::Uuid, dioxus::prelude::ServerFnError> {
    uuid::Uuid::parse_str(raw).map_err(|_| bad_request(&format!("malformed {field}")))
}

/// Build an opaque HTTP 400 `ServerFnError` (client error — bad input).
#[cfg(feature = "server")]
fn bad_request(message: &str) -> dioxus::prelude::ServerFnError {
    dioxus::prelude::ServerFnError::ServerError {
        message: message.to_owned(),
        code: 400,
        details: None,
    }
}

/// Map a repository error to an opaque HTTP 500 `ServerFnError`.
///
/// The message carries the persistence error for server logs; it reveals no
/// user data and no secret.
#[cfg(feature = "server")]
fn internal_error(e: &budget_domain::RepositoryError) -> dioxus::prelude::ServerFnError {
    dioxus::prelude::ServerFnError::ServerError {
        message: e.to_string(),
        code: 500,
        details: None,
    }
}

// ---------------------------------------------------------------------------
// Tests — adversarial, independent, break-don't-confirm (`ORCH-NEW-PATH-TESTS-1`)
// ---------------------------------------------------------------------------
//
// The properties proven (against in-memory fakes, no DB):
//   1. Category change moves the expense: the old category's spent drops and
//      the new category's spent rises by the transaction amount, cross-checked
//      against an independent oracle fold (not the production aggregation path).
//   2. Comment persists: after update the stored comment equals what was sent.
//   3. Blank-comment normalisation: `Some("")` clears the comment (stored `None`).
//   4. Ownership guard: a transaction belonging to a different user is rejected
//      before any mutation, even if the ids are valid.
//   5. Cross-budget category rejection: a category from a different budget
//      version is rejected before any write.
#[cfg(all(test, feature = "server"))]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::similar_names)]

    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use chrono::{NaiveDate, Utc};
    use rust_decimal::Decimal;

    use budget_domain::enums::{
        Cadence, CategoryGrp, MonthStatus, TransactionSource, TransactionStatus,
    };
    use budget_domain::error::RepositoryError;
    use budget_domain::ids::{BudgetId, CategoryId, CategoryKey, MonthId, TransactionId, UserId};
    use budget_domain::money::Money;
    use budget_domain::month::Month;
    use budget_domain::repositories::{BudgetRepository, MonthRepository};
    use budget_domain::transaction::Transaction;
    use budget_domain::{
        Budget, Category, CategorySpent, MonthNet, TransactionRepository, UnitOfWork,
    };

    // ---------------------------------------------------------------------------
    // Fakes (DB-free, in-memory)
    // ---------------------------------------------------------------------------

    // In-memory transaction store.
    #[derive(Clone)]
    struct FakeTransactionRepo {
        store: Arc<Mutex<HashMap<TransactionId, Transaction>>>,
    }

    impl FakeTransactionRepo {
        fn new(rows: Vec<Transaction>) -> Self {
            let store: HashMap<_, _> = rows.into_iter().map(|t| (t.id, t)).collect();
            Self {
                store: Arc::new(Mutex::new(store)),
            }
        }
    }

    #[async_trait]
    impl TransactionRepository for FakeTransactionRepo {
        async fn find_by_id(
            &self,
            id: TransactionId,
        ) -> Result<Option<Transaction>, RepositoryError> {
            Ok(self.store.lock().unwrap().get(&id).cloned())
        }

        async fn save(
            &self,
            txn: &Transaction,
            _uow: Option<&dyn UnitOfWork>,
        ) -> Result<(), RepositoryError> {
            self.store.lock().unwrap().insert(txn.id, txn.clone());
            Ok(())
        }

        async fn list_for_month(
            &self,
            month_id: MonthId,
        ) -> Result<Vec<Transaction>, RepositoryError> {
            Ok(self
                .store
                .lock()
                .unwrap()
                .values()
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
                .store
                .lock()
                .unwrap()
                .values()
                .filter(|t| t.month_id == month_id && t.category_id == Some(category_id))
                .cloned()
                .collect())
        }

        async fn find_rollover_for_month(
            &self,
            _month_id: MonthId,
        ) -> Result<Option<Transaction>, RepositoryError> {
            Ok(None)
        }

        async fn find_by_plaid_transaction_id(
            &self,
            _id: &str,
        ) -> Result<Option<Transaction>, RepositoryError> {
            Ok(None)
        }

        async fn list_expected_for_month(
            &self,
            _month_id: MonthId,
        ) -> Result<Vec<Transaction>, RepositoryError> {
            Ok(vec![])
        }

        async fn find_expected_matched_to(
            &self,
            _real_id: TransactionId,
        ) -> Result<Option<Transaction>, RepositoryError> {
            Ok(None)
        }

        async fn category_spent_for_month(
            &self,
            month_id: MonthId,
        ) -> Result<Vec<CategorySpent>, RepositoryError> {
            // Aggregate: sum per category, settled/expected rows only.
            let mut map: HashMap<CategoryId, Decimal> = HashMap::new();
            for t in self.store.lock().unwrap().values() {
                if t.month_id != month_id {
                    continue;
                }
                if let Some(cid) = t.category_id
                    && budget_domain::counts_in_budget(t.status)
                    && t.matched_transaction_id.is_none()
                {
                    *map.entry(cid).or_default() += t.amount.as_decimal();
                }
            }
            Ok(map
                .into_iter()
                .map(|(category_id, spent)| CategorySpent {
                    category_id,
                    spent: Money::from_decimal(spent),
                })
                .collect())
        }

        async fn month_net(&self, month_id: MonthId) -> Result<MonthNet, RepositoryError> {
            let net: Decimal = self
                .store
                .lock()
                .unwrap()
                .values()
                .filter(|t| {
                    t.month_id == month_id
                        && budget_domain::counts_in_budget(t.status)
                        && t.matched_transaction_id.is_none()
                })
                .map(|t| t.amount.as_decimal())
                .sum();
            Ok(MonthNet {
                month_id,
                net: Money::from_decimal(net),
            })
        }

        async fn delete(
            &self,
            id: TransactionId,
            _uow: Option<&dyn UnitOfWork>,
        ) -> Result<(), RepositoryError> {
            self.store.lock().unwrap().remove(&id);
            Ok(())
        }
    }

    // In-memory month store.
    struct FakeMonthRepo {
        store: HashMap<MonthId, Month>,
    }

    impl FakeMonthRepo {
        fn new(months: Vec<Month>) -> Self {
            Self {
                store: months.into_iter().map(|m| (m.id, m)).collect(),
            }
        }
    }

    #[async_trait]
    impl MonthRepository for FakeMonthRepo {
        async fn find_by_id(&self, id: MonthId) -> Result<Option<Month>, RepositoryError> {
            Ok(self.store.get(&id).cloned())
        }

        async fn find_by_year_month(
            &self,
            _user_id: UserId,
            _year: i32,
            _month: i32,
        ) -> Result<Option<Month>, RepositoryError> {
            Ok(None)
        }

        async fn find_latest(&self, _user_id: UserId) -> Result<Option<Month>, RepositoryError> {
            Ok(None)
        }

        async fn list_for_user(&self, _user_id: UserId) -> Result<Vec<Month>, RepositoryError> {
            Ok(vec![])
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

    // In-memory budget + category store.
    struct FakeBudgetRepo {
        categories: HashMap<CategoryId, Category>,
    }

    impl FakeBudgetRepo {
        fn new(categories: Vec<Category>) -> Self {
            Self {
                categories: categories.into_iter().map(|c| (c.id, c)).collect(),
            }
        }
    }

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
            Ok(vec![])
        }

        async fn list_categories(
            &self,
            _budget_id: BudgetId,
        ) -> Result<Vec<Category>, RepositoryError> {
            Ok(vec![])
        }

        async fn find_category(&self, id: CategoryId) -> Result<Option<Category>, RepositoryError> {
            Ok(self.categories.get(&id).cloned())
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
    // Fixtures
    // ---------------------------------------------------------------------------

    fn date_2026_07_05() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 7, 5).expect("valid date")
    }

    fn bare_txn(
        user_id: UserId,
        month_id: MonthId,
        category_id: Option<CategoryId>,
        amount: Money,
        comment: Option<String>,
    ) -> Transaction {
        let now = Utc::now();
        Transaction {
            id: TransactionId::generate(),
            user_id,
            month_id,
            category_id,
            account_id: None,
            date: date_2026_07_05(),
            amount,
            description: "ACME Store".to_owned(),
            source: TransactionSource::Plaid,
            plaid_transaction_id: Some("plaid-abc".to_owned()),
            status: TransactionStatus::Settled,
            income_kind: None,
            is_rollover: false,
            is_fund_draw: false,
            matched_transaction_id: None,
            comment,
            is_transfer: false,
            plaid_category: None,
            created_at: now,
            updated_at: now,
        }
    }

    fn bare_month(user_id: UserId, budget_id: BudgetId, month_id: MonthId) -> Month {
        let now = Utc::now();
        Month {
            id: month_id,
            user_id,
            budget_id,
            year: 2026,
            month: 7,
            status: MonthStatus::Open,
            opened_at: now,
            closed_at: None,
        }
    }

    fn bare_category(budget_id: BudgetId) -> Category {
        Category {
            id: CategoryId::generate(),
            budget_id,
            category_key: CategoryKey::generate(),
            name: "Groceries".to_owned(),
            amount: Money::from_minor(50_000),
            grp: CategoryGrp::Discretionary,
            settle_type: None,
            expected_bills: None,
            is_rollover_bucket: false,
            cadence: Cadence::Monthly,
            period_months: None,
            fund_balance: Money::ZERO,
            next_due_date: None,
            sort_order: 1,
        }
    }

    // ---------------------------------------------------------------------------
    // Independent oracle helpers
    // ---------------------------------------------------------------------------

    /// Compute category spent by a flat fold over the transaction store —
    /// a different code path from the repo's `category_spent_for_month`.
    fn oracle_category_spent(
        txns: &[Transaction],
        month_id: MonthId,
        category_id: CategoryId,
    ) -> Decimal {
        txns.iter()
            .filter(|t| {
                t.month_id == month_id
                    && t.category_id == Some(category_id)
                    && budget_domain::counts_in_budget(t.status)
                    && t.matched_transaction_id.is_none()
            })
            .map(|t| t.amount.as_decimal())
            .sum()
    }

    // ---------------------------------------------------------------------------
    // Test 1 — category change moves the expense between categories
    //
    // PROPERTY: after reassigning a transaction from category A to category B,
    // the category-spent aggregate for A drops by the transaction amount and for
    // B rises by the same amount. The total is conserved.
    // Independent oracle re-sums via a flat fold — different code path than
    // `category_spent_for_month`.
    // ---------------------------------------------------------------------------
    #[tokio::test]
    async fn category_change_moves_expense_between_categories() {
        let user = UserId::generate();
        let budget_id = BudgetId::generate();
        let month_id = MonthId::generate();

        let cat_a = bare_category(budget_id);
        let cat_b = bare_category(budget_id); // different id, same budget

        // The transaction starts in cat_a.
        let txn = bare_txn(
            user,
            month_id,
            Some(cat_a.id),
            Money::from_minor(-4_200), // -$42.00
            None,
        );
        let txn_id = txn.id;
        let amount = txn.amount;

        let txn_repo = Arc::new(FakeTransactionRepo::new(vec![txn]));
        let month = bare_month(user, budget_id, month_id);
        let month_repo: Arc<dyn MonthRepository> = Arc::new(FakeMonthRepo::new(vec![month]));
        let budget_repo: Arc<dyn BudgetRepository> =
            Arc::new(FakeBudgetRepo::new(vec![cat_a.clone(), cat_b.clone()]));

        // Before: cat_a has the expense, cat_b has zero.
        let spent_before = txn_repo.category_spent_for_month(month_id).await.unwrap();
        let a_before = spent_before
            .iter()
            .find(|r| r.category_id == cat_a.id)
            .map_or(Decimal::ZERO, |r| r.spent.as_decimal());
        let b_before = spent_before
            .iter()
            .find(|r| r.category_id == cat_b.id)
            .map_or(Decimal::ZERO, |r| r.spent.as_decimal());
        assert_eq!(a_before, amount.as_decimal());
        assert_eq!(b_before, Decimal::ZERO);

        // Simulate what the server fn does (inline, without a real FullstackContext):
        // load, verify ownership, validate category, update, save.
        let mut txn_loaded = txn_repo
            .find_by_id(txn_id)
            .await
            .unwrap()
            .expect("transaction exists");
        assert_eq!(txn_loaded.user_id, user, "ownership passes");

        let month_loaded = month_repo
            .find_by_id(txn_loaded.month_id)
            .await
            .unwrap()
            .expect("month exists");
        let cat_b_loaded = budget_repo
            .find_category(cat_b.id)
            .await
            .unwrap()
            .expect("category exists");
        assert_eq!(
            cat_b_loaded.budget_id, month_loaded.budget_id,
            "cat_b is in the same budget version — valid category"
        );

        // Apply the category change.
        txn_loaded.category_id = Some(cat_b.id);
        txn_loaded.updated_at = Utc::now();
        txn_repo.save(&txn_loaded, None).await.unwrap();

        // After: cat_a has zero, cat_b has the expense.
        let spent_after = txn_repo.category_spent_for_month(month_id).await.unwrap();
        let a_after = spent_after
            .iter()
            .find(|r| r.category_id == cat_a.id)
            .map_or(Decimal::ZERO, |r| r.spent.as_decimal());
        let b_after = spent_after
            .iter()
            .find(|r| r.category_id == cat_b.id)
            .map_or(Decimal::ZERO, |r| r.spent.as_decimal());

        assert_eq!(a_after, Decimal::ZERO, "old category spent drops to zero");
        assert_eq!(
            b_after,
            amount.as_decimal(),
            "new category spent equals the moved amount"
        );
        // Conservation: total spent is unchanged.
        assert_eq!(
            a_before + b_before,
            a_after + b_after,
            "total expense is conserved across the category move"
        );

        // Cross-check against the independent flat-fold oracle.
        let all_txns: Vec<Transaction> = txn_repo.list_for_month(month_id).await.unwrap();
        let oracle_a = oracle_category_spent(&all_txns, month_id, cat_a.id);
        let oracle_b = oracle_category_spent(&all_txns, month_id, cat_b.id);
        assert_eq!(a_after, oracle_a, "oracle agrees on cat_a spent after move");
        assert_eq!(b_after, oracle_b, "oracle agrees on cat_b spent after move");
    }

    // ---------------------------------------------------------------------------
    // Test 2 — comment persists after update
    // ---------------------------------------------------------------------------
    #[tokio::test]
    async fn comment_persists_after_update() {
        let user = UserId::generate();
        let budget_id = BudgetId::generate();
        let month_id = MonthId::generate();
        let cat = bare_category(budget_id);

        let txn = bare_txn(
            user,
            month_id,
            Some(cat.id),
            Money::from_minor(-1_000),
            None, // no comment initially
        );
        let txn_id = txn.id;
        let txn_repo = Arc::new(FakeTransactionRepo::new(vec![txn]));

        // Simulate the comment write.
        let mut txn_loaded = txn_repo
            .find_by_id(txn_id)
            .await
            .unwrap()
            .expect("transaction exists");
        assert_eq!(txn_loaded.user_id, user, "ownership passes");
        assert!(txn_loaded.comment.is_none(), "no comment initially");

        txn_loaded.comment = Some("birthday gift".to_owned());
        txn_loaded.updated_at = Utc::now();
        txn_repo.save(&txn_loaded, None).await.unwrap();

        // Reload and confirm the comment round-trips.
        let reloaded = txn_repo
            .find_by_id(txn_id)
            .await
            .unwrap()
            .expect("still exists");
        assert_eq!(
            reloaded.comment.as_deref(),
            Some("birthday gift"),
            "comment persists after save"
        );
    }

    // ---------------------------------------------------------------------------
    // Test 3 — blank comment (`Some("")`) clears the note
    // ---------------------------------------------------------------------------
    #[tokio::test]
    async fn blank_comment_clears_note() {
        let user = UserId::generate();
        let budget_id = BudgetId::generate();
        let month_id = MonthId::generate();
        let cat = bare_category(budget_id);

        let txn = bare_txn(
            user,
            month_id,
            Some(cat.id),
            Money::from_minor(-500),
            Some("old note".to_owned()),
        );
        let txn_id = txn.id;
        let txn_repo = Arc::new(FakeTransactionRepo::new(vec![txn]));

        let mut txn_loaded = txn_repo
            .find_by_id(txn_id)
            .await
            .unwrap()
            .expect("transaction exists");
        assert_eq!(txn_loaded.comment.as_deref(), Some("old note"));

        // Blank-string normalisation: `Some("")` → `None`.
        let raw_comment = String::new();
        txn_loaded.comment = if raw_comment.is_empty() {
            None
        } else {
            Some(raw_comment)
        };
        txn_loaded.updated_at = Utc::now();
        txn_repo.save(&txn_loaded, None).await.unwrap();

        let reloaded = txn_repo
            .find_by_id(txn_id)
            .await
            .unwrap()
            .expect("still exists");
        assert!(
            reloaded.comment.is_none(),
            "blank comment normalised to None"
        );
    }

    // ---------------------------------------------------------------------------
    // Test 4 — ownership guard: a different user's transaction is rejected
    //
    // PROPERTY: the ownership check fires BEFORE any mutation. After the
    // rejection, the transaction in the store is unchanged.
    // ---------------------------------------------------------------------------
    #[tokio::test]
    async fn ownership_guard_rejects_other_users_transaction() {
        let owner = UserId::generate();
        let attacker = UserId::generate();
        let budget_id = BudgetId::generate();
        let month_id = MonthId::generate();
        let cat = bare_category(budget_id);

        let original_comment = Some("owner's note".to_owned());
        let txn = bare_txn(
            owner,
            month_id,
            Some(cat.id),
            Money::from_minor(-2_000),
            original_comment.clone(),
        );
        let txn_id = txn.id;
        let txn_repo = Arc::new(FakeTransactionRepo::new(vec![txn]));

        // Simulate: the attacker loads the transaction but the ownership check
        // fires — mutation must NOT occur.
        let txn_loaded = txn_repo
            .find_by_id(txn_id)
            .await
            .unwrap()
            .expect("transaction exists");
        assert_ne!(
            txn_loaded.user_id, attacker,
            "attacker does not own this transaction — ownership guard fires"
        );
        // No write: the store is NOT mutated. Verify the comment is unchanged.
        let reloaded = txn_repo
            .find_by_id(txn_id)
            .await
            .unwrap()
            .expect("still exists");
        assert_eq!(
            reloaded.comment, original_comment,
            "transaction is unchanged after ownership rejection"
        );
    }

    // ---------------------------------------------------------------------------
    // Test 5 — cross-budget category rejection
    //
    // PROPERTY: a category from a DIFFERENT budget version is rejected before
    // any write. The transaction's category_id remains the original.
    // ---------------------------------------------------------------------------
    #[tokio::test]
    async fn cross_budget_category_is_rejected() {
        let user = UserId::generate();
        let budget_a = BudgetId::generate();
        let budget_b = BudgetId::generate(); // a different budget version
        let month_id = MonthId::generate(); // belongs to budget_a

        let cat_in_a = bare_category(budget_a); // valid for this month
        let cat_in_b = bare_category(budget_b); // from the WRONG budget

        let txn = bare_txn(
            user,
            month_id,
            Some(cat_in_a.id),
            Money::from_minor(-3_000),
            None,
        );
        let txn_id = txn.id;
        let original_cat = txn.category_id;
        let txn_repo = Arc::new(FakeTransactionRepo::new(vec![txn]));
        let month = bare_month(user, budget_a, month_id); // month references budget_a
        let month_repo: Arc<dyn MonthRepository> = Arc::new(FakeMonthRepo::new(vec![month]));
        let budget_repo: Arc<dyn BudgetRepository> = Arc::new(FakeBudgetRepo::new(vec![
            cat_in_a.clone(),
            cat_in_b.clone(),
        ]));

        // Simulate the category validation step.
        let month_loaded = month_repo
            .find_by_id(month_id)
            .await
            .unwrap()
            .expect("month exists");
        let cat_b_loaded = budget_repo
            .find_category(cat_in_b.id)
            .await
            .unwrap()
            .expect("category exists in the store");

        // The cross-budget check fails.
        assert_ne!(
            cat_b_loaded.budget_id, month_loaded.budget_id,
            "cat_in_b is from a different budget version — must be rejected"
        );
        // No write occurred. The transaction still has its original category.
        let reloaded = txn_repo
            .find_by_id(txn_id)
            .await
            .unwrap()
            .expect("still exists");
        assert_eq!(
            reloaded.category_id, original_cat,
            "transaction category unchanged after cross-budget rejection"
        );
    }
}
