//! `get_month_view` server function — the core read-side of the budget tracker
//! (`SPEC §4.3`, `§4.5`, `§6`, `§7`, `BUDGET-AUTH-GATE-1`, `RUST-DIOXUS-9`).
//!
//! Returns the per-category budget/spent/remaining summary for one month, plus
//! the rolling "Other" balance and month metadata. The caller supplies `(year,
//! month)` and receives a [`MonthViewDto`] suitable for rendering by the chorale
//! table in the UI.
//!
//! ## Computation rules applied here
//!
//! - `BUDGET-STATUS-DRIVES-INCLUSION-1` — category spent uses only
//!   settled + expected transactions (via `category_spent_for_month`, which
//!   pushes the SQL filter down to one query, `DB-NPLUSONE-1`).
//! - `BUDGET-NO-DOUBLE-CHARGE-1` — fixed-category spent =
//!   `settled ? sum(txns) : placeholder`, never both.
//! - `BUDGET-MONEY-1` — every monetary value is `rust_decimal::Decimal`
//!   serialised to a `String`; no float crosses any boundary.
//! - `BUDGET-AUTH-GATE-1` — `require_authed_user()` is the very first call.
//!
//! Also exposes [`ensure_month`], a lightweight server function that fires
//! `MonthLifecycleService::ensure_current_month` on page load so the lazy-init
//! (`BUDGET-IDEMPOTENT-MONTH-INIT-1`) runs before the view fetch.

use dioxus::prelude::*;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// DTOs (compile on both targets — WASM-clean)
// ---------------------------------------------------------------------------

/// The full summary for one budget month — what the UI renders.
///
/// Money amounts are serialised as `String` (exact decimal, `BUDGET-MONEY-1`):
/// the `rust_decimal::Decimal` newtype inside `Money` serialises transparently
/// as a decimal string, which is round-trip exact and carries no float.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MonthViewDto {
    /// Calendar year of this month.
    pub year: i32,
    /// Calendar month (1-12).
    pub month: i32,
    /// Human-readable month label ("July 2026").
    pub label: String,
    /// Whether the month is still open.
    pub is_open: bool,
    /// One row per category in `sort_order`.
    pub categories: Vec<CategoryRowDto>,
    /// The rolling "Other" balance visible at the top of the UI.
    ///
    /// This is the sum of the rollover bucket category rows (the `is_rollover_bucket`
    /// category): the rollover transaction (prior-month carryover) plus any
    /// in-month Other-bucket entries (fund contributions, manual charges).
    /// Formatted as a signed currency string for display.
    pub rolling_other: String,
    /// `true` if the month record exists in the DB (lazy-init has run).
    pub month_exists: bool,
}

/// One row in the categories table.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CategoryRowDto {
    /// Display name.
    pub name: String,
    /// Monthly budgeted amount (positive, formatted as currency).
    pub budgeted: String,
    /// Actual spent this month, per `BUDGET-NO-DOUBLE-CHARGE-1`
    /// (placeholder or sum, signed — negative = outflow).
    pub spent: String,
    /// `budgeted + spent` (signed; positive = under budget).
    pub remaining: String,
    /// `true` for the rollover ("Other") bucket category.
    pub is_rollover: bool,
    /// Settle state label for display ("settled" / "unsettled" / "n/a").
    pub settle_state: String,
}

// ---------------------------------------------------------------------------
// Server-function (native only; stripped on wasm32 by the `#[server]` macro)
// ---------------------------------------------------------------------------

/// Ensure the current month is initialised (lazy-init, `BUDGET-IDEMPOTENT-MONTH-INIT-1`).
///
/// Call this once on page load before `get_month_view`. It is idempotent: if the
/// month already exists it is a fast no-op. Gated by `require_authed_user`.
///
/// # Errors
///
/// `ServerFnError` if the session is absent (401) or any persistence call fails.
#[allow(clippy::unused_async)]
#[server]
pub async fn ensure_month() -> Result<(), dioxus::prelude::ServerFnError> {
    use chrono::Utc;

    use crate::server_state::MonthViewState;
    use crate::services::gate::require_authed_user;

    let user = require_authed_user().await?;
    let state = MonthViewState::extract().await?;
    state
        .lifecycle
        .ensure_current_month(user.id(), Utc::now())
        .await
        .map_err(|e| dioxus::prelude::ServerFnError::ServerError {
            message: e.to_string(),
            code: 500,
            details: None,
        })?;
    Ok(())
}

/// Fetch the budget summary for `(year, month)`, gated by session auth
/// (`BUDGET-AUTH-GATE-1`).
///
/// Returns a [`MonthViewDto`] with per-category budget / spent / remaining, the
/// rolling Other balance, and month metadata. If no month record exists yet
/// (lazy-init has not run for this month), returns an empty view rather than an
/// error so the UI can prompt the user to navigate to the current month.
///
/// # Errors
///
/// `ServerFnError` (HTTP 401) when there is no valid session. Other errors are
/// returned as HTTP 500 with the error message.
#[allow(clippy::unused_async, clippy::too_many_lines)]
#[server]
pub async fn get_month_view(
    year: i32,
    month: i32,
) -> Result<MonthViewDto, dioxus::prelude::ServerFnError> {
    use std::collections::HashMap;

    use budget_domain::enums::{CategoryGrp, MonthStatus, SettleType};
    use budget_domain::money::Money;
    use budget_domain::predicates::{FixedSettlement, fixed_category_spent};

    use crate::server_state::MonthViewState;
    use crate::services::gate::require_authed_user;

    let user = require_authed_user().await?;
    let state = MonthViewState::extract().await?;

    // Resolve the DB month record (may be absent if lazy-init hasn't run).
    let maybe_month = state
        .months
        .find_by_year_month(user.id(), year, month)
        .await
        .map_err(|e| dioxus::prelude::ServerFnError::ServerError {
            message: e.to_string(),
            code: 500,
            details: None,
        })?;

    let label = month_label(year, month);

    let Some(db_month) = maybe_month else {
        // Month not yet initialised — return an empty shell.
        return Ok(MonthViewDto {
            year,
            month,
            label,
            is_open: false,
            categories: vec![],
            rolling_other: format_money_zero(),
            month_exists: false,
        });
    };

    // Load categories for the budget version that month references.
    let categories = state
        .budgets
        .list_categories(db_month.budget_id)
        .await
        .map_err(|e| dioxus::prelude::ServerFnError::ServerError {
            message: e.to_string(),
            code: 500,
            details: None,
        })?;

    // Single-query aggregation: per-category spent totals for the month
    // (`DB-NPLUSONE-1`, `BUDGET-STATUS-DRIVES-INCLUSION-1`).
    let spent_rows = state
        .transactions
        .category_spent_for_month(db_month.id)
        .await
        .map_err(|e| dioxus::prelude::ServerFnError::ServerError {
            message: e.to_string(),
            code: 500,
            details: None,
        })?;

    // Build a lookup: category_id -> signed sum of counting transactions.
    let spent_map: HashMap<_, _> = spent_rows
        .into_iter()
        .map(|cs| (cs.category_id, cs.spent))
        .collect();

    let mut rolling_other = Money::ZERO;
    let mut category_rows: Vec<CategoryRowDto> = Vec::with_capacity(categories.len());

    for cat in &categories {
        // Transaction sum for this category (zero if no counting transactions).
        let txn_sum = spent_map.get(&cat.id).copied().unwrap_or(Money::ZERO);

        // `BUDGET-NO-DOUBLE-CHARGE-1`: fixed-category spent = settled ? sum : placeholder.
        let spent = if cat.grp == CategoryGrp::Fixed {
            // Determine settlement: a non-zero txn_sum means at least one
            // counting (settled/expected) transaction was assigned. The SQL
            // aggregate already applied the counting predicate, so this is a
            // safe proxy for the read-only view.
            let settlement = if txn_sum == Money::ZERO {
                FixedSettlement::Unsettled
            } else {
                FixedSettlement::Settled
            };
            // placeholder is the budgeted amount negated (expense = outflow).
            let placeholder = -cat.amount;
            fixed_category_spent(settlement, placeholder, txn_sum)
        } else {
            // Discretionary: just the transaction sum.
            txn_sum
        };

        // remaining = budgeted (positive) + spent (negative outflow = under budget).
        let budgeted_positive = cat.amount;
        let remaining = budgeted_positive + spent;

        // Settle state label for display.
        let settle_state = match (cat.grp, cat.settle_type) {
            (CategoryGrp::Fixed, Some(SettleType::FlexibleSet)) => {
                if spent == txn_sum && txn_sum != Money::ZERO {
                    "settled".to_owned()
                } else {
                    "unsettled".to_owned()
                }
            }
            (CategoryGrp::Fixed, Some(SettleType::TrueSet)) => "true-set".to_owned(),
            (CategoryGrp::Fixed, None) => "fixed".to_owned(),
            (CategoryGrp::Discretionary, _) => "n/a".to_owned(),
        };

        // The rollover bucket category's spent = the rolling Other balance.
        if cat.is_rollover_bucket {
            rolling_other = spent;
        }

        category_rows.push(CategoryRowDto {
            name: cat.name.clone(),
            budgeted: format_money(budgeted_positive),
            spent: format_money(spent),
            remaining: format_money(remaining),
            is_rollover: cat.is_rollover_bucket,
            settle_state,
        });
    }

    Ok(MonthViewDto {
        year,
        month,
        label,
        is_open: db_month.status == MonthStatus::Open,
        categories: category_rows,
        rolling_other: format_money(rolling_other),
        month_exists: true,
    })
}

// ---------------------------------------------------------------------------
// Helpers (compile on both targets — pure functions, no I/O)
// ---------------------------------------------------------------------------

/// Format a `Money` value as a signed currency string ("$1,234.56" / "-$42.00").
///
/// Uses `rust_decimal::Decimal` arithmetic only (`BUDGET-MONEY-1`, no float).
/// The currency symbol and comma-grouping are hand-rolled to avoid pulling a
/// locale library into the WASM bundle.
#[cfg(feature = "server")]
fn format_money(m: budget_domain::money::Money) -> String {
    format_decimal(m.as_decimal())
}

#[cfg(feature = "server")]
fn format_money_zero() -> String {
    "$0.00".to_owned()
}

/// Format a `rust_decimal::Decimal` as a USD currency string.
///
/// Negative values prefix with "-$"; positive/zero with "$". Two decimal places,
/// comma thousands-grouping. Pure: no I/O, no float (`BUDGET-MONEY-1`).
#[cfg(feature = "server")]
fn format_decimal(d: rust_decimal::Decimal) -> String {
    use rust_decimal::prelude::ToPrimitive;

    let negative = d.is_sign_negative();
    let abs_d = d.abs().round_dp(2);

    // Split into integer and fractional parts.
    let int_part = abs_d.trunc().to_u64().unwrap_or(0);
    let frac_part = ((abs_d.fract()) * rust_decimal::Decimal::new(100, 0))
        .round_dp(0)
        .to_u64()
        .unwrap_or(0);

    // Comma-group the integer part.
    let int_str = int_part.to_string();
    let mut grouped = String::new();
    for (i, ch) in int_str.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(ch);
    }
    let grouped: String = grouped.chars().rev().collect();

    let body = format!("{grouped}.{frac_part:02}");
    if negative {
        format!("-${body}")
    } else {
        format!("${body}")
    }
}

/// Human-readable "Month Year" label for display (server-side only).
///
/// Called from the `#[server]` bodies which are stripped on the wasm32 target;
/// gating avoids a dead-code warning on the client build.
#[cfg(feature = "server")]
fn month_label(year: i32, month: i32) -> String {
    let name = match month {
        1 => "January",
        2 => "February",
        3 => "March",
        4 => "April",
        5 => "May",
        6 => "June",
        7 => "July",
        8 => "August",
        9 => "September",
        10 => "October",
        11 => "November",
        12 => "December",
        _ => "Unknown",
    };
    format!("{name} {year}")
}
