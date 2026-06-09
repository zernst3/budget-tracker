//! The month-lifecycle service — lazy idempotent month-init + rolling-Other
//! rollover (`SPEC §4.6`, `§4.3`; the core differentiator, build step 4).
//!
//! ## Lazy init (`BUDGET-IDEMPOTENT-MONTH-INIT-1`)
//!
//! On access, [`MonthLifecycleService::ensure_current_month`] finds the latest
//! existing month, then creates **every** missing month up to the current one
//! **in chronological order**, resolving the correct budget version per month
//! and posting each month's rollover. It handles a multi-month gap (the
//! scale-to-zero container may have been asleep across several month
//! boundaries, `SPEC §4.6`) and is idempotent + concurrency-safe:
//!   - month creation goes through
//!     [`budget_domain::repositories::MonthRepository::create_if_absent`], which
//!     is `INSERT ... ON CONFLICT (user_id, year, month) DO NOTHING` backed by
//!     the `UNIQUE(user_id, year, month)` index (`§12`), so two racing inits
//!     converge on one row;
//!   - rollover posting is guarded by [`Self::post_rollover_if_absent`], which
//!     checks `find_rollover_for_month` first and treats the partial-unique
//!     `transactions(month_id) WHERE is_rollover` violation as "already posted",
//!     so re-entry never double-posts (`BUDGET-ROLLOVER-INTEGRITY-1`).
//!
//! ## Rollover as a system transaction (`BUDGET-ROLLOVER-INTEGRITY-1`)
//!
//! The carryover into a month is materialised as a single `transactions` row
//! with `is_rollover = true`, `category = the rollover bucket`, dated the 1st,
//! `amount = the prior month's net leftover`. It is **never** a mutated scalar;
//! the rolling balance is always the sum of this auditable chain.
//!
//! ## Net-leftover formula (`D5`, `§12`)
//!
//! ```text
//! net = (actual_income - expected_income) + Σ(expense category remaining)
//! ```
//!
//! computed by [`net_leftover`]. `expected_income` comes from the
//! [`crate::income::IncomeExpectation`] seam (build step 6 fills it). Fund
//! contributions are **excluded** from the net (`D6`,
//! `BUDGET-FUND-EARMARK-1`): an earmarked dollar is counted once, as the
//! month's fund expense, and never again as Other surplus.
//!
//! ## Timezone (`D2`, `ARCH-UTC-TIMESTAMPS-1`)
//!
//! Month-membership (which `(year, month)` "now" belongs to) is computed in the
//! fixed home timezone `America/New_York`; all stored timestamps remain UTC.
//!
//! ## Transactionality (`SERVICE-TX-1`, `REPO-10`)
//!
//! Each month's "create the month + post its rollover" is one atomic unit run
//! through the [`budget_domain::uow::UowProvider`] closure, so a month is never
//! left created-but-rollover-unposted. The service holds `Arc<dyn _>` repo
//! dependencies (`SERVICE-DI-1`); no `db.*` access lives here.

use std::sync::Arc;

use chrono::{DateTime, Datelike, NaiveDate, Utc};
use chrono_tz::America::New_York;

use budget_domain::RepositoryError;
use budget_domain::enums::{MonthStatus, TransactionSource, TransactionStatus};
use budget_domain::error::DomainError;
use budget_domain::ids::{MonthId, UserId};
use budget_domain::money::Money;
use budget_domain::month::Month;
use budget_domain::predicates::{counts_in_budget, counts_in_month_expense_remaining};
use budget_domain::repositories::{
    BudgetRepository, FundRepository, MonthRepository, TransactionRepository,
};
use budget_domain::transaction::Transaction;
use budget_domain::uow::{UnitOfWork, UowProvider, UowProviderExt};

use crate::income::IncomeExpectation;

/// `Σ(expense category remaining)` over a month's transactions, per the `D5`
/// formula, **excluding fund contributions** (`D6`, `BUDGET-FUND-EARMARK-1`).
///
/// "Remaining" here is the *signed actual spend* counted toward budget math.
/// Each transaction contributes its signed `amount` (expense negative, inflow
/// positive) iff [`counts_in_month_expense_remaining`] is `true`
/// (`BUDGET-STATUS-DRIVES-INCLUSION-1` + `BUDGET-FUND-EARMARK-1`), with these
/// exclusions that the D5 net handles in its other terms or not at all:
///   - **income rows** (`income_kind` set) — they belong to the
///     `(actual_income - expected_income)` term, not here;
///   - **fund-contribution rows** — money moved into a sinking fund / buffer /
///     surplus is already an expense against the month and is excluded from the
///     net so the earmarked dollar is counted once (`BUDGET-FUND-EARMARK-1`);
///   - **buffer-financed full-price rows** — a buffer-financed purchase posts for
///     tracking only with zero month-budget impact; the budget effect is its
///     installments, so the full-price row is excluded (`SPEC §4.9` D7);
///   - the **rollover row of *this* month** is naturally part of the prior
///     chain when present, and IS included: it is a real signed line item in
///     Other, exactly the auditable carryover (`BUDGET-ROLLOVER-INTEGRITY-1`).
///
/// `fund_category_ids` is the set of category ids in the month's budget version
/// whose category is a sinking fund (`Category::is_sinking_fund`); a transaction
/// assigned to one of those categories is a fund contribution when its amount is
/// an outflow. `buffer_financed_txn_ids` is the set of buffer-financed full-price
/// purchase transactions (`SPEC §4.9` D7), which post for tracking only and carry
/// zero month-budget impact.
///
/// Both exclusions are decided by the single
/// [`counts_in_month_expense_remaining`] domain predicate so the netting here and
/// the fund service (build step 5) cannot drift.
#[must_use]
fn expense_remaining_sum(
    transactions: &[Transaction],
    fund_category_ids: &[budget_domain::ids::CategoryId],
    buffer_financed_txn_ids: &[budget_domain::ids::TransactionId],
) -> Money {
    transactions
        .iter()
        .filter(|t| {
            counts_in_month_expense_remaining(t, fund_category_ids, buffer_financed_txn_ids)
        })
        .map(|t| t.amount)
        .sum()
}

/// `actual_income` for a month: the signed sum of budget-counting income rows
/// (`income_kind` set, `SPEC §4.8`), per the `D5` formula.
#[must_use]
fn actual_income_sum(transactions: &[Transaction]) -> Money {
    transactions
        .iter()
        .filter(|t| counts_in_budget(t.status))
        .filter(|t| t.is_income())
        .map(|t| t.amount)
        .sum()
}

/// The `D5` net-leftover formula (`§12`, `BUDGET-ROLLOVER-INTEGRITY-1`):
///
/// ```text
/// net = (actual_income - expected_income) + Σ(expense category remaining)
/// ```
///
/// A pure function over the three already-derived figures so the formula is
/// single-source and unit-testable in isolation. `expense_remaining` is the
/// fund-excluded signed expense sum (`BUDGET-FUND-EARMARK-1`); income variance
/// nets in by formula (`D5`), counted once.
#[must_use]
pub fn net_leftover(
    actual_income: Money,
    expected_income: Money,
    expense_remaining: Money,
) -> Money {
    (actual_income - expected_income) + expense_remaining
}

/// Lazy month-init + rolling-Other rollover (`SPEC §4.6`, `§4.3`).
///
/// Holds `Arc<dyn _>` repository + provider dependencies (`SERVICE-DI-1`); all
/// `db.*` lives in the repositories. The income expectation is the build-step-6
/// seam (`crate::income`).
pub struct MonthLifecycleService {
    months: Arc<dyn MonthRepository>,
    budgets: Arc<dyn BudgetRepository>,
    transactions: Arc<dyn TransactionRepository>,
    funds: Arc<dyn FundRepository>,
    uow: Arc<dyn UowProvider>,
    income: Arc<dyn IncomeExpectation>,
}

impl MonthLifecycleService {
    /// Wire the service from its dependencies (`SERVICE-DI-1`).
    #[must_use]
    pub fn new(
        months: Arc<dyn MonthRepository>,
        budgets: Arc<dyn BudgetRepository>,
        transactions: Arc<dyn TransactionRepository>,
        funds: Arc<dyn FundRepository>,
        uow: Arc<dyn UowProvider>,
        income: Arc<dyn IncomeExpectation>,
    ) -> Self {
        Self {
            months,
            budgets,
            transactions,
            funds,
            uow,
            income,
        }
    }

    /// Ensure the user's current month exists, catching up every missing month
    /// in order and posting each rollover (`BUDGET-IDEMPOTENT-MONTH-INIT-1`).
    ///
    /// Resolves the current `(year, month)` in `America/New_York` from `now`
    /// (`D2`), then drives the catch-up from the latest existing month. Returns
    /// the current month. Idempotent: re-running is a no-op once everything is
    /// in place.
    ///
    /// `now` is injected (rather than read from the wall clock inside) so the
    /// catch-up is deterministically testable; production callers pass
    /// [`Utc::now`].
    ///
    /// # Errors
    /// [`DomainError`] on any persistence failure or when no budget version
    /// covers a month that must be created (`SPEC §4.1`).
    pub async fn ensure_current_month(
        &self,
        user_id: UserId,
        now: DateTime<Utc>,
    ) -> Result<Month, DomainError> {
        let (target_year, target_month) = year_month_in_home_tz(now);

        // The anchor for catch-up: the latest existing month, or — on a brand
        // new account with no months at all — the target month itself (nothing
        // to roll over into the very first month).
        let latest = self.months.find_latest(user_id).await?;
        let (mut cursor_year, mut cursor_month) = match &latest {
            Some(m) => next_month(m.year, m.month),
            None => (target_year, target_month),
        };

        // Create every month from the cursor up to and including the target, in
        // chronological order, posting each one's rollover from the prior month.
        // The loop is bounded by the target; a multi-month gap is just more
        // iterations (SPEC §4.6).
        while (cursor_year, cursor_month) <= (target_year, target_month) {
            self.ensure_month(user_id, cursor_year, cursor_month, now)
                .await?;
            let (ny, nm) = next_month(cursor_year, cursor_month);
            cursor_year = ny;
            cursor_month = nm;
        }

        // Re-read the now-guaranteed current month.
        self.months
            .find_by_year_month(user_id, target_year, target_month)
            .await?
            .ok_or_else(|| {
                DomainError::IllegalState(format!(
                    "current month {target_year}-{target_month:02} absent after lazy-init"
                ))
            })
    }

    /// Create one `(year, month)` for the user (if absent) and post its rollover
    /// from the immediately prior month, atomically (`SERVICE-TX-1`).
    ///
    /// Resolves the budget version active on the 1st of the target month
    /// (`SPEC §4.1`), then runs the create + rollover post inside one
    /// transaction so a month is never left without its rollover.
    async fn ensure_month(
        &self,
        user_id: UserId,
        year: i32,
        month: i32,
        now: DateTime<Utc>,
    ) -> Result<(), DomainError> {
        let first_of_month = first_of_month(year, month)?;

        // The budget version active on the 1st of the target month decides the
        // month's budget_id and supplies the rollover bucket (SPEC §4.1).
        let budget = self
            .budgets
            .find_active_for_date(user_id, first_of_month)
            .await?
            .ok_or_else(|| {
                DomainError::Invariant(format!(
                    "no budget version active on {first_of_month} for {user_id}"
                ))
            })?;

        // Compute the rollover amount BEFORE opening the write tx: it is a pure
        // read over the prior month, with no write dependency, so it stays
        // outside the transaction to keep the tx short.
        let rollover_amount = self.prior_month_net(user_id, year, month).await?;

        let new_month = Month {
            id: MonthId::generate(),
            user_id,
            budget_id: budget.id,
            year,
            month,
            status: MonthStatus::Open,
            opened_at: now,
            closed_at: None,
        };

        // Resolve the rollover bucket category up front; the rollover row posts
        // against it (BUDGET-ROLLOVER-INTEGRITY-1).
        let bucket = self
            .budgets
            .find_rollover_bucket(budget.id)
            .await?
            .ok_or_else(|| {
                DomainError::Invariant(format!(
                    "budget version {} has no rollover bucket",
                    budget.id
                ))
            })?;

        // SERVICE-TX-1 / REPO-10: create-month + post-rollover is one atomic
        // unit. create_if_absent is ON CONFLICT DO NOTHING; the rollover post is
        // pre-checked + partial-unique-guarded, so the whole closure is
        // re-entry-safe and a partial month is impossible.
        let months = Arc::clone(&self.months);
        let transactions = Arc::clone(&self.transactions);
        let bucket_id = bucket.id;
        let month_to_create = new_month.clone();

        self.uow
            .run(move |uow: &dyn UnitOfWork| {
                Box::pin(async move {
                    let resolved = months.create_if_absent(&month_to_create, Some(uow)).await?;
                    post_rollover_if_absent(
                        transactions.as_ref(),
                        user_id,
                        resolved.id,
                        bucket_id,
                        first_of_month,
                        rollover_amount,
                        now,
                        Some(uow),
                    )
                    .await?;
                    Ok(())
                })
            })
            .await?;

        Ok(())
    }

    /// The net leftover of the month immediately before `(year, month)` — the
    /// amount that rolls forward into `(year, month)`'s Other
    /// (`SPEC §4.3`, `D5`).
    ///
    /// Returns [`Money::ZERO`] when there is no prior month (the genesis month),
    /// so the first month opens with a zero rollover.
    async fn prior_month_net(
        &self,
        user_id: UserId,
        year: i32,
        month: i32,
    ) -> Result<Money, DomainError> {
        let (py, pm) = prev_month(year, month);
        let Some(prior) = self.months.find_by_year_month(user_id, py, pm).await? else {
            // Genesis: no prior month, nothing to roll over.
            return Ok(Money::ZERO);
        };

        // The prior month's budget version supplies which categories are funds
        // (BUDGET-FUND-EARMARK-1: fund contributions are excluded from the net).
        let categories = self.budgets.list_categories(prior.budget_id).await?;
        let fund_ids: Vec<_> = categories
            .iter()
            .filter(|c| c.is_sinking_fund())
            .map(|c| c.id)
            .collect();

        let txns = self.transactions.list_for_month(prior.id).await?;

        // SPEC §4.9 D7: buffer-financed full-price purchases post for tracking
        // only with zero month-budget impact; they are excluded from the expense
        // sum so they never blow up their month. The installments (ordinary
        // expenses) are included and net normally.
        let buffer_financed_txn_ids = self
            .funds
            .list_buffer_financed_transaction_ids(user_id)
            .await?;

        let actual_income = actual_income_sum(&txns);
        let expected_income = self.income.expected_income(user_id, py, pm);
        let expense_remaining = expense_remaining_sum(&txns, &fund_ids, &buffer_financed_txn_ids);

        Ok(net_leftover(
            actual_income,
            expected_income,
            expense_remaining,
        ))
    }
}

/// Post a month's rollover transaction iff it is not already present
/// (`BUDGET-ROLLOVER-INTEGRITY-1`, `BUDGET-IDEMPOTENT-MONTH-INIT-1`).
///
/// Idempotency is two-layered:
///   1. a `find_rollover_for_month` pre-check short-circuits the common
///      re-entry case, and
///   2. the partial-unique `transactions(month_id) WHERE is_rollover` index is
///      the concurrency-safe backstop: if a racing init inserts the rollover
///      between our check and our insert, the insert fails with a
///      [`RepositoryError::UniqueViolation`], which we treat as "already
///      posted" — a benign no-op, not an error.
///
/// The row is the canonical system rollover: `is_rollover = true`, category =
/// the rollover bucket, dated the 1st, `amount` = the prior month's net leftover
/// (never a mutated scalar).
#[allow(clippy::too_many_arguments)]
async fn post_rollover_if_absent(
    transactions: &dyn TransactionRepository,
    user_id: UserId,
    month_id: MonthId,
    bucket_id: budget_domain::ids::CategoryId,
    first_of_month: NaiveDate,
    amount: Money,
    now: DateTime<Utc>,
    uow: Option<&dyn UnitOfWork>,
) -> Result<(), RepositoryError> {
    // Layer 1: pre-check. The single is_rollover=true row per month.
    if transactions
        .find_rollover_for_month(month_id)
        .await?
        .is_some()
    {
        return Ok(());
    }

    let rollover = Transaction {
        id: budget_domain::ids::TransactionId::generate(),
        user_id,
        month_id,
        category_id: Some(bucket_id),
        account_id: None,
        date: first_of_month,
        amount,
        description: "Rollover from prior month".to_owned(),
        source: TransactionSource::Manual,
        plaid_transaction_id: None,
        status: TransactionStatus::Settled,
        income_kind: None,
        is_rollover: true,
        is_fund_draw: false,
        created_at: now,
        updated_at: now,
    };

    // Layer 2: partial-unique backstop. A racing insert that wins the
    // transactions(month_id) WHERE is_rollover race surfaces as a
    // UniqueViolation, which we treat as a benign "already posted" no-op rather
    // than a double-post (BUDGET-ROLLOVER-INTEGRITY-1).
    match transactions.save(&rollover, uow).await {
        // Ok = we posted it; UniqueViolation = a racing init already did. Both
        // mean "the rollover now exists exactly once", so both are success.
        Ok(()) | Err(RepositoryError::UniqueViolation(_)) => Ok(()),
        Err(e) => Err(e),
    }
}

/// The calendar `(year, month)` that a UTC instant belongs to, computed in the
/// fixed home timezone `America/New_York` (`D2`, `ARCH-UTC-TIMESTAMPS-1`).
///
/// The instant is stored UTC; only month-membership is evaluated in the home
/// TZ, so an instant just after midnight on the 1st in New York is correctly the
/// new month even though it is still the prior day in UTC.
#[must_use]
fn year_month_in_home_tz(now: DateTime<Utc>) -> (i32, i32) {
    let local = now.with_timezone(&New_York);
    (local.year(), i32::try_from(local.month()).unwrap_or(1))
}

/// The 1st of `(year, month)` as a [`NaiveDate`]; the rollover row's date and
/// the budget-version resolution date (`SPEC §4.1`, `§4.3`).
fn first_of_month(year: i32, month: i32) -> Result<NaiveDate, DomainError> {
    let m = u32::try_from(month)
        .ok()
        .filter(|m| (1..=12).contains(m))
        .ok_or_else(|| DomainError::IllegalState(format!("month {month} out of range")))?;
    NaiveDate::from_ymd_opt(year, m, 1)
        .ok_or_else(|| DomainError::IllegalState(format!("invalid date {year}-{month:02}-01")))
}

/// The `(year, month)` immediately after `(year, month)` (December wraps to
/// January of the next year).
#[must_use]
const fn next_month(year: i32, month: i32) -> (i32, i32) {
    if month >= 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    }
}

/// The `(year, month)` immediately before `(year, month)` (January wraps to
/// December of the prior year).
#[must_use]
const fn prev_month(year: i32, month: i32) -> (i32, i32) {
    if month <= 1 {
        (year - 1, 12)
    } else {
        (year, month - 1)
    }
}

#[cfg(test)]
mod tests;
