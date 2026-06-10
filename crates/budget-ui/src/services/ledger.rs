//! Month-ledger + envelope-summary server functions — the BACKEND-2 read side
//! of the month ledger (`SPEC §7`, `§4.3`, `§4.5`, `§4.2`, `BUDGET-AUTH-GATE-1`,
//! `RUST-DIOXUS-9`).
//!
//! Two gated, read-only server functions, each extracting the authenticated user
//! FIRST (`BUDGET-AUTH-GATE-1`) and scoping every query to that user:
//!
//!   - [`get_month_ledger`] — the daily ledger (`SPEC §7`): one entry per day
//!     that has expense activity, each carrying the day's total expense AND its
//!     individual transactions (so the UI can render the day rows + the grouped
//!     child table). Income rows and the system rollover row are excluded from a
//!     day's *expense* total but the transactions themselves are returned with
//!     flags so the UI can present them.
//!   - [`get_envelope_summary`] — the collapsible envelope-summary header
//!     (`SPEC §7`): per-category budgeted / spent / remaining (settled ? sum :
//!     placeholder, `BUDGET-NO-DOUBLE-CHARGE-1`) plus the rolling-Other balance
//!     (`SPEC §4.3`).
//!
//! ## Money representation (`BUDGET-MONEY-1`)
//!
//! Every monetary field on these DTOs is [`budget_domain::Money`] — a newtype
//! over `rust_decimal::Decimal`. The Decimal is the source of truth and crosses
//! the wire intact (serde-transparent, exact). The UI converts to `f64` ONLY at
//! the chorale `CellValue::Float` accessor boundary for display/aggregation; no
//! float is computed or stored here. (This is the deliberate divergence from the
//! older B4 [`super::month_view`] DTOs, which pre-format to display strings; the
//! ledger keeps Decimal end-to-end so chorale can aggregate day/category
//! subtotals itself.)
//!
//! ## Aggregation rules applied here
//!
//! - `BUDGET-STATUS-DRIVES-INCLUSION-1` — only settled + expected transactions
//!   count toward spent / day totals; `pending` is excluded
//!   (`budget_domain::counts_in_budget`).
//! - `BUDGET-NO-DOUBLE-CHARGE-1` — fixed-category spent =
//!   `settled ? sum : placeholder`, via `budget_domain::envelope_category_spent`.
//! - `BUDGET-SETTLE-ON-MATCH-1` — a matched expected placeholder is excluded from
//!   budget math (the real transaction counts instead), so a matched/real pair
//!   counts exactly once.

use budget_domain::Money;
use dioxus::prelude::*;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// DTOs (compile on both targets — WASM-clean; Money is Decimal-backed)
// ---------------------------------------------------------------------------

/// The full daily ledger for one month (`SPEC §7`).
///
/// One [`DayLedgerDto`] per day that has at least one transaction, ordered by
/// date ascending. `month_total_expense` is the sum of every day's
/// `total_expense` — the month's total expense, the figure the chorale day-total
/// column aggregates up to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MonthLedgerDto {
    /// Calendar year of this month.
    pub year: i32,
    /// Calendar month (1-12).
    pub month: i32,
    /// `true` if the month record exists in the DB (lazy-init has run). When
    /// `false` the `days` list is empty and the UI should prompt navigation to a
    /// current month.
    pub month_exists: bool,
    /// One entry per day with activity, ascending by date.
    pub days: Vec<DayLedgerDto>,
    /// Sum of every day's `total_expense` (signed; negative = net outflow). Equal
    /// to `Σ days[].total_expense` by construction — the month's expense total.
    pub month_total_expense: Money,
}

/// One day's ledger row (`SPEC §7`): the day's total expense plus its
/// individual transactions for the drill-down child table.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DayLedgerDto {
    /// The day (ISO `YYYY-MM-DD`).
    pub date: String,
    /// The day's total EXPENSE (signed; negative = outflow), the sum of this
    /// day's budget-counting, non-income, non-rollover transaction amounts. This
    /// is what the day row displays; income / rollover rows are present in
    /// `transactions` but excluded from this total (`SPEC §7`: the day row is the
    /// "when did I spend" view).
    pub total_expense: Money,
    /// The day's transactions (every status), for the grouped child table.
    pub transactions: Vec<LedgerTransactionDto>,
}

/// One transaction in a day's child table (`SPEC §7`). The two inline-editable
/// fields (`category` / `comment`) are carried as the current value; the UI
/// edits them via separate mutation server functions (a later phase).
///
/// The four boolean fields are independent display facts the UI renders/filters
/// on (uncategorized affordance, rollover/income styling, budget-counting),
/// not a state machine — so the `struct_excessive_bools` lint is allowed here.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LedgerTransactionDto {
    /// Stable transaction id (the edit/mutation target).
    pub id: String,
    /// Transaction date (ISO `YYYY-MM-DD`).
    pub date: String,
    /// Assigned category id (`None` = uncategorized — should not appear in the
    /// ledger normally, but surfaced rather than hidden).
    pub category_id: Option<String>,
    /// Assigned category display name; `None` when uncategorized.
    pub category_name: Option<String>,
    /// `true` when no category is assigned (`category_id IS NULL`) — the inline
    /// category dropdown still needs to render for these.
    pub uncategorized: bool,
    /// Signed amount (`Money`; negative = expense, positive = inflow).
    pub amount: Money,
    /// Plaid / merchant description (read-only).
    pub description: String,
    /// User free-text note (`transactions.comment`); `None` = no note.
    pub comment: Option<String>,
    /// Settlement status as a lowercase label ("settled" / "expected" /
    /// "pending"). The UI filters/sorts on it (`SPEC §7`).
    pub status: String,
    /// `true` for the system-generated rollover line item (read-only, not an
    /// expense of the day).
    pub is_rollover: bool,
    /// `true` for an income inflow row.
    pub is_income: bool,
    /// `true` when this counts toward budget math
    /// (`BUDGET-STATUS-DRIVES-INCLUSION-1`): settled/expected and not a matched
    /// placeholder. The day total only sums rows for which this is `true` AND
    /// which are not income/rollover.
    pub counts_in_budget: bool,
}

/// The envelope-summary header for one month (`SPEC §7`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnvelopeSummaryDto {
    /// Calendar year.
    pub year: i32,
    /// Calendar month (1-12).
    pub month: i32,
    /// `true` if the month record exists (lazy-init has run).
    pub month_exists: bool,
    /// `true` while the month is open.
    pub is_open: bool,
    /// One row per category in `sort_order`.
    pub categories: Vec<EnvelopeCategoryDto>,
    /// The rolling "Other" balance (`SPEC §4.3`): the rollover bucket category's
    /// spent (prior-month carryover plus any in-month Other entries). Signed.
    pub rolling_other: Money,
}

/// One category's envelope row (`SPEC §7`): budgeted / spent / remaining.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnvelopeCategoryDto {
    /// Category id.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Budgeted amount (positive).
    pub budgeted: Money,
    /// Spent this month (signed; negative = outflow), per
    /// `BUDGET-NO-DOUBLE-CHARGE-1` (settled ? sum : placeholder for fixed
    /// categories; raw sum for discretionary; balance for the rollover bucket).
    pub spent: Money,
    /// `budgeted + spent` (signed; positive = under budget / remaining).
    pub remaining: Money,
    /// `true` for the rollover ("Other") bucket category.
    pub is_rollover: bool,
}

// ---------------------------------------------------------------------------
// Server functions (native only; the `#[server]` macro strips the body on wasm)
// ---------------------------------------------------------------------------

/// Fetch the daily ledger for `(year, month)`, gated by session auth
/// (`BUDGET-AUTH-GATE-1`).
///
/// Returns one [`DayLedgerDto`] per day with activity (ascending), each carrying
/// its day total and the day's individual transactions grouped-ready for the
/// chorale child table. If the month record does not exist yet, returns an empty
/// ledger (`month_exists = false`) rather than an error.
///
/// Day-total semantics (`SPEC §7`): a day's `total_expense` sums only the day's
/// budget-counting (`BUDGET-STATUS-DRIVES-INCLUSION-1`), non-income, non-rollover
/// transactions. The rollover row (dated the 1st) and income rows are still
/// returned in that day's `transactions` (flagged) so the child table can show
/// them, but they do not inflate the day's expense figure. `month_total_expense`
/// is the sum of the per-day totals and therefore equals the month's expense
/// total by construction.
///
/// # Errors
///
/// `ServerFnError` (HTTP 401) when there is no valid session; HTTP 500 on any
/// persistence failure.
#[allow(clippy::unused_async, clippy::too_many_lines)]
#[server]
pub async fn get_month_ledger(
    year: i32,
    month: i32,
) -> Result<MonthLedgerDto, dioxus::prelude::ServerFnError> {
    use std::collections::HashMap;

    use budget_domain::CategoryId;

    use crate::server_state::MonthViewState;
    use crate::services::gate::require_authed_user;

    let user = require_authed_user().await?;
    let state = MonthViewState::extract().await?;

    // Resolve the DB month record (may be absent if lazy-init hasn't run).
    let Some(db_month) = state
        .months
        .find_by_year_month(user.id(), year, month)
        .await
        .map_err(|e| internal_error(&e))?
    else {
        return Ok(MonthLedgerDto {
            year,
            month,
            month_exists: false,
            days: vec![],
            month_total_expense: Money::ZERO,
        });
    };

    // Category-name lookup for the child-table grouping headers. Scoped to the
    // month's budget version (the version the month references, SPEC §4.1).
    let categories = state
        .budgets
        .list_categories(db_month.budget_id)
        .await
        .map_err(|e| internal_error(&e))?;
    let name_by_id: HashMap<CategoryId, String> =
        categories.iter().map(|c| (c.id, c.name.clone())).collect();

    // Every transaction in the month (all statuses); the day-grouping core
    // applies the inclusion predicate per row for the day-total decision.
    let txns = state
        .transactions
        .list_for_month(db_month.id)
        .await
        .map_err(|e| internal_error(&e))?;

    // The buffer-financed full-price tracking rows (SPEC §4.9 D7): post for
    // TRACKING only and must NOT inflate the day/month expense totals (the budget
    // effect is the installments). Same exclusion set the close-path netting uses,
    // so the ledger totals match `counts_in_month_expense_remaining`.
    let buffer_financed = state
        .funds
        .list_buffer_financed_transaction_ids(user.id())
        .await
        .map_err(|e| internal_error(&e))?;

    let (days, month_total) = build_days(&txns, &name_by_id, &buffer_financed);

    Ok(MonthLedgerDto {
        year,
        month,
        month_exists: true,
        days,
        month_total_expense: month_total,
    })
}

/// Pure day-grouping core (`SPEC §7`): group the month's transactions by day,
/// compute each day's expense total, and the month total.
///
/// Extracted from the server function so the day-total invariant is unit-tested
/// directly (no DB / session plumbing). Days are returned ascending by date
/// (`BTreeMap` ordering). A day's `total_expense` sums ONLY the transactions that
/// pass [`budget_domain::counts_in_month_expense_remaining`] — the SAME predicate
/// the month-close netting uses (`BUDGET-STATUS-DRIVES-INCLUSION-1`,
/// `BUDGET-NO-DOUBLE-CHARGE-1`, `BUDGET-FUND-EARMARK-1`): budget-counting by
/// status, not income, not a matched placeholder (`BUDGET-SETTLE-ON-MATCH-1`),
/// **not a fund draw** (`is_fund_draw`, already expensed at contribution time), and
/// **not a buffer-financed full-price tracking row** (`buffer_financed_txn_ids`,
/// SPEC §4.9 D7 — the budget effect is the installments). The rollover row is
/// excluded because `counts_in_month_expense_remaining` treats it as income-side
/// netting carryover, not a day expense. The returned `month_total_expense` is the
/// sum of those per-day totals, so `Σ days[].total_expense == month_total` holds by
/// construction.
#[cfg(feature = "server")]
fn build_days(
    txns: &[budget_domain::Transaction],
    name_by_id: &std::collections::HashMap<budget_domain::CategoryId, String>,
    buffer_financed_txn_ids: &[budget_domain::ids::TransactionId],
) -> (Vec<DayLedgerDto>, Money) {
    use std::collections::BTreeMap;

    use budget_domain::TransactionStatus;
    use budget_domain::counts_in_month_expense_remaining;

    let mut by_day: BTreeMap<chrono::NaiveDate, Vec<LedgerTransactionDto>> = BTreeMap::new();
    let mut day_total: BTreeMap<chrono::NaiveDate, Money> = BTreeMap::new();

    for t in txns {
        let is_income = t.is_income();
        // Counts toward the day's EXPENSE total iff the shared close-path predicate
        // includes it: status counts, not a matched placeholder
        // (BUDGET-SETTLE-ON-MATCH-1), not income, not a fund draw
        // (BUDGET-NO-DOUBLE-CHARGE-1: the money was already expensed at contribution
        // time), and not a buffer-financed full-price tracking row (SPEC §4.9 D7:
        // the installments are the budget effect, not the full price). `&[]` for the
        // fund-category arg: D6 Model A no longer drives a contribution exclusion off
        // it (it is unused by the predicate). Reusing the predicate keeps the ledger
        // totals from drifting away from the month-close net.
        let counts =
            counts_in_month_expense_remaining(t, &[], buffer_financed_txn_ids) && !t.is_rollover;
        if counts {
            *day_total.entry(t.date).or_insert(Money::ZERO) += t.amount;
        }

        // The per-row display flag (BUDGET-STATUS-DRIVES-INCLUSION-1): status-counting
        // and not a matched placeholder. This is a row-level "does this line item
        // count by status" badge, distinct from the day-total inclusion above (which
        // additionally drops income / rollover / fund-draw / buffer-financed rows).
        let row_counts_in_budget = t.counts_in_budget() && !t.is_matched_placeholder();

        let status = match t.status {
            TransactionStatus::Settled => "settled",
            TransactionStatus::Expected => "expected",
            TransactionStatus::Pending => "pending",
        };

        by_day
            .entry(t.date)
            .or_default()
            .push(LedgerTransactionDto {
                id: t.id.to_string(),
                date: t.date.to_string(),
                category_id: t.category_id.map(|c| c.to_string()),
                category_name: t.category_id.and_then(|c| name_by_id.get(&c).cloned()),
                uncategorized: t.category_id.is_none(),
                amount: t.amount,
                description: t.description.clone(),
                comment: t.comment.clone(),
                status: status.to_owned(),
                is_rollover: t.is_rollover,
                is_income,
                counts_in_budget: row_counts_in_budget,
            });
    }

    let mut days: Vec<DayLedgerDto> = Vec::with_capacity(by_day.len());
    let mut month_total = Money::ZERO;
    for (date, transactions) in by_day {
        let total = day_total.get(&date).copied().unwrap_or(Money::ZERO);
        month_total += total;
        days.push(DayLedgerDto {
            date: date.to_string(),
            total_expense: total,
            transactions,
        });
    }

    (days, month_total)
}

/// Fetch the envelope-summary header for `(year, month)`, gated by session auth
/// (`BUDGET-AUTH-GATE-1`).
///
/// Returns per-category budgeted / spent / remaining (settled ? sum :
/// placeholder, `BUDGET-NO-DOUBLE-CHARGE-1`) plus the rolling-Other balance
/// (`SPEC §4.3`). If the month record does not exist yet, returns an empty
/// summary (`month_exists = false`).
///
/// Spent is computed from a single per-category SQL aggregate
/// (`category_spent_for_month`, `DB-NPLUSONE-1`) whose status filter is the
/// inclusion polarity of `BUDGET-STATUS-DRIVES-INCLUSION-1` (settled + expected;
/// pending and matched placeholders excluded), then run through the pure
/// `budget_domain::envelope_category_spent` predicate. Matched expected
/// placeholders are excluded by the SQL aggregate (`AND matched_transaction_id IS
/// NULL`), so a matched placeholder is never double-counted against its real
/// charge (`BUDGET-SETTLE-ON-MATCH-1`).
///
/// # Errors
///
/// `ServerFnError` (HTTP 401) when there is no valid session; HTTP 500 on any
/// persistence failure.
#[allow(clippy::unused_async)]
#[server]
pub async fn get_envelope_summary(
    year: i32,
    month: i32,
) -> Result<EnvelopeSummaryDto, dioxus::prelude::ServerFnError> {
    use std::collections::HashMap;

    use budget_domain::{CategoryId, MonthStatus};

    use crate::server_state::MonthViewState;
    use crate::services::gate::require_authed_user;

    let user = require_authed_user().await?;
    let state = MonthViewState::extract().await?;

    let Some(db_month) = state
        .months
        .find_by_year_month(user.id(), year, month)
        .await
        .map_err(|e| internal_error(&e))?
    else {
        return Ok(EnvelopeSummaryDto {
            year,
            month,
            month_exists: false,
            is_open: false,
            categories: vec![],
            rolling_other: Money::ZERO,
        });
    };

    let categories = state
        .budgets
        .list_categories(db_month.budget_id)
        .await
        .map_err(|e| internal_error(&e))?;

    // Single-query per-category counting sums (DB-NPLUSONE-1). The aggregate
    // already applies the inclusion polarity and excludes matched placeholders
    // (BUDGET-SETTLE-ON-MATCH-1) in SQL.
    let spent_rows = state
        .transactions
        .category_spent_for_month(db_month.id)
        .await
        .map_err(|e| internal_error(&e))?;
    let spent_map: HashMap<CategoryId, Money> = spent_rows
        .into_iter()
        .map(|cs| (cs.category_id, cs.spent))
        .collect();

    let (rows, rolling_other) = build_envelope_rows(&categories, &spent_map);

    Ok(EnvelopeSummaryDto {
        year,
        month,
        month_exists: true,
        is_open: db_month.status == MonthStatus::Open,
        categories: rows,
        rolling_other,
    })
}

/// Pure envelope-row core (`SPEC §7` / `§4.5`): turn the categories + their
/// per-category counting sums into the envelope rows + the rolling-Other balance.
///
/// Extracted from the server function so the `settled ? sum : placeholder`
/// classification and the rolling-Other resolution are unit-tested directly. Each
/// row's `spent` is `budget_domain::envelope_category_spent` (the single tested
/// predicate); `remaining = budgeted + spent`. `rolling_other` is the rollover
/// bucket category's spent (`SPEC §4.3`).
#[cfg(feature = "server")]
fn build_envelope_rows(
    categories: &[budget_domain::Category],
    spent_map: &std::collections::HashMap<budget_domain::CategoryId, Money>,
) -> (Vec<EnvelopeCategoryDto>, Money) {
    use budget_domain::envelope_category_spent;

    let mut rolling_other = Money::ZERO;
    let mut rows: Vec<EnvelopeCategoryDto> = Vec::with_capacity(categories.len());

    for cat in categories {
        let counting_sum = spent_map.get(&cat.id).copied().unwrap_or(Money::ZERO);
        // BUDGET-NO-DOUBLE-CHARGE-1 / SPEC §4.5, in one tested domain predicate.
        let spent = envelope_category_spent(cat, counting_sum);
        let remaining = cat.amount + spent;

        if cat.is_rollover_bucket {
            rolling_other = spent;
        }

        rows.push(EnvelopeCategoryDto {
            id: cat.id.to_string(),
            name: cat.name.clone(),
            budgeted: cat.amount,
            spent,
            remaining,
            is_rollover: cat.is_rollover_bucket,
        });
    }

    (rows, rolling_other)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Map a repository error to an opaque HTTP 500 `ServerFnError`.
///
/// The message carries the persistence error text for server logs; it reveals no
/// user data and no secret. Server-only (called from `#[server]` bodies, stripped
/// on wasm).
#[cfg(feature = "server")]
fn internal_error(e: &budget_domain::RepositoryError) -> dioxus::prelude::ServerFnError {
    dioxus::prelude::ServerFnError::ServerError {
        message: e.to_string(),
        code: 500,
        details: None,
    }
}

// ---------------------------------------------------------------------------
// Tests — adversarial, independent oracles (ORCH-NEW-PATH-TESTS-1)
// ---------------------------------------------------------------------------
//
// These exercise the PURE aggregation cores (`build_days`, `build_envelope_rows`)
// directly — no DB / session plumbing. The cores are exactly the logic the gated
// server functions run after their repo reads, so testing them proves the
// correctness claims the BACKEND-2 task asks for:
//   1. per-category spent matches `settled ? sum : placeholder`,
//   2. day totals sum to the month's expense total,
//   3. matched expected-expense placeholders are not double-counted
//      (BUDGET-SETTLE-ON-MATCH-1).
// Each numeric assertion is cross-checked against an INDEPENDENT oracle that
// re-derives the figure by a different route (a raw `rust_decimal` fold, or the
// naive double-count), so a copy of the production logic cannot tautologically
// green the test.
#[cfg(all(test, feature = "server"))]
mod tests {
    // Tests construct fixtures and assert on resolved values; `unwrap`/`expect` on
    // known-valid constructors is the established test pattern (see auth_gate.rs).
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::similar_names)]

    use std::collections::HashMap;

    use budget_domain::ids::{BudgetId, CategoryKey};
    use budget_domain::{
        Cadence, Category, CategoryGrp, CategoryId, IncomeKind, Money, MonthId, Transaction,
        TransactionSource, TransactionStatus, UserId,
    };
    use chrono::{NaiveDate, Utc};
    use rust_decimal::Decimal;

    use super::{build_days, build_envelope_rows};

    fn day(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).expect("valid date")
    }

    /// A bare settled expense transaction on a given date / category / amount.
    fn txn(date: NaiveDate, category_id: Option<CategoryId>, amount: Money) -> Transaction {
        let now = Utc::now();
        Transaction {
            id: budget_domain::TransactionId::generate(),
            user_id: UserId::generate(),
            month_id: MonthId::generate(),
            category_id,
            account_id: None,
            date,
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
            is_transfer: false,
            plaid_category: None,
            created_at: now,
            updated_at: now,
        }
    }

    fn category(grp: CategoryGrp, amount: Money, is_rollover_bucket: bool) -> Category {
        Category {
            id: CategoryId::generate(),
            budget_id: BudgetId::generate(),
            category_key: CategoryKey::generate(),
            name: "cat".to_owned(),
            amount,
            grp,
            settle_type: None,
            expected_bills: None,
            is_rollover_bucket,
            cadence: Cadence::Monthly,
            period_months: None,
            fund_balance: Money::ZERO,
            next_due_date: None,
            sort_order: 0,
        }
    }

    /// Independent oracle: the month's expense total computed by a flat
    /// `rust_decimal` fold over the rows the SPEC says count (settled/expected,
    /// not income, not rollover, not a matched placeholder) — a different code
    /// path from `build_days`'s grouped accumulation.
    fn oracle_month_expense(txns: &[Transaction]) -> Decimal {
        txns.iter()
            .filter(|t| {
                t.counts_in_budget()
                    && !t.is_matched_placeholder()
                    && !t.is_income()
                    && !t.is_rollover
            })
            .map(|t| t.amount.as_decimal())
            .sum()
    }

    #[test]
    fn day_totals_sum_to_the_month_expense_total() {
        // Three days of activity; assert per-day totals AND Σ days == month total,
        // cross-checked against the independent fold oracle.
        let cat = CategoryId::generate();
        let txns = vec![
            txn(day(2026, 7, 3), Some(cat), Money::from_minor(-1_000)), // -$10
            txn(day(2026, 7, 3), Some(cat), Money::from_minor(-2_550)), // -$25.50
            txn(day(2026, 7, 10), Some(cat), Money::from_minor(-4_000)), // -$40
            txn(day(2026, 7, 21), Some(cat), Money::from_minor(-999)),  // -$9.99
        ];
        let names: HashMap<CategoryId, String> = HashMap::new();

        let (days, month_total) = build_days(&txns, &names, &[]);

        // Days are ascending and one per active day.
        assert_eq!(days.len(), 3);
        assert_eq!(days[0].date, "2026-07-03");
        assert_eq!(days[1].date, "2026-07-10");
        assert_eq!(days[2].date, "2026-07-21");
        assert_eq!(days[0].total_expense, Money::from_minor(-3_550));
        assert_eq!(days[1].total_expense, Money::from_minor(-4_000));
        assert_eq!(days[2].total_expense, Money::from_minor(-999));

        // INVARIANT: Σ days[].total_expense == month_total_expense.
        let summed: Money = days.iter().map(|d| d.total_expense).sum();
        assert_eq!(summed, month_total);
        // Independent oracle (flat fold) agrees with the grouped total.
        assert_eq!(month_total.as_decimal(), oracle_month_expense(&txns));
        assert_eq!(month_total, Money::from_minor(-8_549));
    }

    #[test]
    fn pending_income_and_rollover_are_excluded_from_day_totals_but_present_in_rows() {
        let cat = CategoryId::generate();
        let mut pending = txn(day(2026, 7, 5), Some(cat), Money::from_minor(-5_000));
        pending.status = TransactionStatus::Pending; // excluded from budget math
        let mut income = txn(day(2026, 7, 5), Some(cat), Money::from_minor(300_000));
        income.income_kind = Some(IncomeKind::Budgeted); // excluded from expense total
        let mut rollover = txn(day(2026, 7, 1), Some(cat), Money::from_minor(21_200));
        rollover.is_rollover = true; // excluded from expense total
        let real = txn(day(2026, 7, 5), Some(cat), Money::from_minor(-1_234));

        let txns = vec![pending, income, rollover, real];
        let names: HashMap<CategoryId, String> = HashMap::new();
        let (days, month_total) = build_days(&txns, &names, &[]);

        // The only expense counted is the real -$12.34, on July 5.
        assert_eq!(month_total, Money::from_minor(-1_234));
        assert_eq!(month_total.as_decimal(), oracle_month_expense(&txns));

        // But every row (including pending / income / rollover) is still RETURNED
        // for the child table, flagged appropriately.
        let total_rows: usize = days.iter().map(|d| d.transactions.len()).sum();
        assert_eq!(total_rows, 4, "all rows surfaced, none hidden");
        let day5 = days
            .iter()
            .find(|d| d.date == "2026-07-05")
            .expect("july 5 present");
        assert!(
            day5.transactions
                .iter()
                .any(|t| t.status == "pending" && !t.counts_in_budget)
        );
        assert!(day5.transactions.iter().any(|t| t.is_income));
        let day1 = days
            .iter()
            .find(|d| d.date == "2026-07-01")
            .expect("july 1 present");
        assert!(day1.transactions.iter().any(|t| t.is_rollover));
        // The rollover day has rows but a ZERO expense total.
        assert_eq!(day1.total_expense, Money::ZERO);
    }

    #[test]
    fn matched_expected_placeholder_not_double_counted_with_its_real_charge() {
        // BUDGET-SETTLE-ON-MATCH-1: an expected placeholder matched to a real
        // charge drops out of the day total; only the real charge counts, so the
        // pair counts EXACTLY ONCE.
        let cat = CategoryId::generate();
        let real = txn(day(2026, 7, 9), Some(cat), Money::from_minor(-8_000));
        let mut placeholder = txn(day(2026, 7, 9), Some(cat), Money::from_minor(-8_000));
        placeholder.status = TransactionStatus::Expected;
        placeholder.matched_transaction_id = Some(real.id);
        assert!(placeholder.is_matched_placeholder());

        let txns = vec![real, placeholder];
        let names: HashMap<CategoryId, String> = HashMap::new();
        let (_days, month_total) = build_days(&txns, &names, &[]);

        // Counts once (-$80), NOT twice (-$160 would be the double-count bug).
        assert_eq!(month_total, Money::from_minor(-8_000));
        assert_ne!(month_total, Money::from_minor(-16_000));
        assert_eq!(month_total.as_decimal(), oracle_month_expense(&txns));
    }

    #[test]
    fn fund_draw_and_buffer_financed_rows_are_excluded_from_day_totals() {
        // BUDGET-NO-DOUBLE-CHARGE-1 / SPEC §4.9 D7: after triage, a PayFromSavings
        // row carries is_fund_draw=true (money already expensed at contribution
        // time) and a SpreadOverMonths full-price tracking row is referenced by a
        // repayment obligation. NEITHER may inflate the day/month expense total —
        // only the ordinary expense counts. This mirrors the month-close net so the
        // ledger and the rollover math agree.
        let cat = CategoryId::generate();

        // An ordinary in-month expense (PayDirectly) — counts: -$30.
        let direct = txn(day(2026, 7, 8), Some(cat), Money::from_minor(-3_000));

        // A fund-draw (PayFromSavings) — categorized + settled but is_fund_draw — must
        // NOT count.
        let mut fund_draw = txn(day(2026, 7, 8), Some(cat), Money::from_minor(-5_000));
        fund_draw.is_fund_draw = true;

        // A buffer-financed full-price tracking row (SpreadOverMonths) — uncategorized,
        // is_fund_draw=false, but referenced by an obligation — must NOT count.
        let buffer_financed = txn(day(2026, 7, 8), None, Money::from_minor(-200_000));
        let buffer_ids = vec![buffer_financed.id];

        let txns = vec![direct, fund_draw, buffer_financed];
        let names: HashMap<CategoryId, String> = HashMap::new();
        let (days, month_total) = build_days(&txns, &names, &buffer_ids);

        // Only the -$30 ordinary expense counts; the draw (-$50) and the
        // full-price (-$2000) are excluded.
        assert_eq!(month_total, Money::from_minor(-3_000));
        // Independent fold over the SAME D6/D7 rules, different code path.
        let oracle: Decimal = txns
            .iter()
            .filter(|t| {
                t.counts_in_budget()
                    && !t.is_matched_placeholder()
                    && !t.is_income()
                    && !t.is_rollover
                    && !t.is_fund_draw
                    && !buffer_ids.contains(&t.id)
            })
            .map(|t| t.amount.as_decimal())
            .sum();
        assert_eq!(month_total.as_decimal(), oracle);

        // The naive double-count bug (summing everything) would be -$2080 — assert
        // we are NOT that.
        assert_ne!(month_total, Money::from_minor(-208_000));

        // All three rows are still surfaced in the child table (read-only display).
        let total_rows: usize = days.iter().map(|d| d.transactions.len()).sum();
        assert_eq!(total_rows, 3, "every row surfaced, none hidden");
    }

    #[test]
    fn category_names_populate_child_rows_and_uncategorized_is_flagged() {
        let cat = CategoryId::generate();
        let mut names = HashMap::new();
        names.insert(cat, "Groceries".to_owned());

        let categorized = txn(day(2026, 7, 2), Some(cat), Money::from_minor(-2_000));
        let uncategorized = txn(day(2026, 7, 2), None, Money::from_minor(-1_000));
        let (days, _) = build_days(&[categorized, uncategorized], &names, &[]);

        let rows = &days[0].transactions;
        let g = rows
            .iter()
            .find(|r| r.category_name.as_deref() == Some("Groceries"))
            .expect("named row present");
        assert!(!g.uncategorized);
        let u = rows
            .iter()
            .find(|r| r.uncategorized)
            .expect("uncategorized row");
        assert!(u.category_id.is_none());
        assert!(u.category_name.is_none());
    }

    // -- envelope -----------------------------------------------------------

    /// Independent oracle for a fixed category's spent: re-implements
    /// `settled ? sum : placeholder` inline (no call to the production
    /// predicate), so the test cannot be tautological.
    fn oracle_fixed_spent(amount: Money, counting_sum: Money) -> Money {
        if counting_sum == Money::ZERO {
            Money::from_decimal(-amount.as_decimal()) // placeholder
        } else {
            counting_sum // settled -> the real sum, NOT placeholder + sum
        }
    }

    #[test]
    fn envelope_spent_matches_settled_sum_or_placeholder_predicate() {
        // Two fixed categories: one unsettled (no txns -> placeholder), one
        // settled (a real charge -> the real sum); one discretionary (raw sum).
        let rent = category(CategoryGrp::Fixed, Money::from_minor(200_000), false); // $2000
        let utilities = category(CategoryGrp::Fixed, Money::from_minor(15_000), false); // $150
        let groceries = category(CategoryGrp::Discretionary, Money::from_minor(50_000), false);

        let mut spent_map: HashMap<CategoryId, Money> = HashMap::new();
        // rent unsettled (absent from the map).
        spent_map.insert(utilities.id, Money::from_minor(-14_500)); // settled $145
        spent_map.insert(groceries.id, Money::from_minor(-31_277)); // $312.77

        let cats = vec![rent.clone(), utilities.clone(), groceries.clone()];
        let (rows, rolling) = build_envelope_rows(&cats, &spent_map);

        assert_eq!(rolling, Money::ZERO, "no rollover bucket present");

        let rent_row = &rows[0];
        // Unsettled fixed -> placeholder (-$2000), provably not the (absent) sum.
        assert_eq!(rent_row.spent, oracle_fixed_spent(rent.amount, Money::ZERO));
        assert_eq!(rent_row.spent, Money::from_minor(-200_000));
        // remaining = budgeted + spent = 2000 + (-2000) = 0.
        assert_eq!(rent_row.remaining, Money::ZERO);

        let util_row = &rows[1];
        // Settled fixed -> the real sum (-$145), NOT placeholder + sum.
        assert_eq!(
            util_row.spent,
            oracle_fixed_spent(utilities.amount, Money::from_minor(-14_500))
        );
        assert_eq!(util_row.spent, Money::from_minor(-14_500));
        assert_ne!(
            util_row.spent,
            Money::from_minor(-15_000) + Money::from_minor(-14_500),
            "must NOT be placeholder + sum (the double-charge bug)"
        );
        // remaining = 150 + (-145) = +$5 under budget.
        assert_eq!(util_row.remaining, Money::from_minor(500));

        let groc_row = &rows[2];
        // Discretionary -> raw sum.
        assert_eq!(groc_row.spent, Money::from_minor(-31_277));
    }

    #[test]
    fn rolling_other_is_the_rollover_bucket_balance() {
        let other = category(CategoryGrp::Discretionary, Money::ZERO, true);
        let groceries = category(CategoryGrp::Discretionary, Money::from_minor(50_000), false);

        let mut spent_map: HashMap<CategoryId, Money> = HashMap::new();
        spent_map.insert(other.id, Money::from_minor(21_200)); // +$212 carryover
        spent_map.insert(groceries.id, Money::from_minor(-30_000));

        let (rows, rolling) = build_envelope_rows(&[other.clone(), groceries], &spent_map);
        // rolling_other == the rollover bucket category's spent, verbatim.
        assert_eq!(rolling, Money::from_minor(21_200));
        let other_row = rows.iter().find(|r| r.is_rollover).expect("rollover row");
        assert_eq!(other_row.spent, Money::from_minor(21_200));
    }
}
