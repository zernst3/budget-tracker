//! Deficit financing (`SPEC §12` D9, `§4.9`, `BUDGET-DEFICIT-FINANCING-1`).
//!
//! ## Default behavior is unchanged
//!
//! A closed month's net deficit rolls forward IN FULL as next month's negative
//! Other carry (`BUDGET-ROLLOVER-INTEGRITY-1`, `SPEC §4.3`). That is the
//! [`crate::month_lifecycle::MonthLifecycleService`] rollover path and it is the
//! only behavior unless the user *electively* finances the deficit.
//!
//! ## The elective option (this module)
//!
//! When a closed month's deficit exceeds a configurable threshold (default 75%)
//! of the NEXT month's Other budget, the app may OFFER (never force) converting
//! the deficit into a [`RepaymentObligation`] amortized over N months — the SAME
//! machinery as a buffer-financed large purchase (`SPEC §4.9` D7), except the
//! principal is the accumulated deficit and there is no single source transaction
//! (`source = deficit`, `origin_month_id` set, `transaction_id = None`).
//!
//! [`DeficitFinancingService::detect_financeable_deficit`] is the threshold check
//! (returns `Some(offer)` only when over threshold);
//! [`DeficitFinancingService::finance_deficit`] performs the conversion.
//!
//! ## How "next month absorbs only installment 1" works (the count-once invariant)
//!
//! Financing does two things atomically (`SERVICE-TX-1`):
//!   1. creates the obligation (`source = deficit`, `origin_month_id = closed
//!      month`, principal = the deficit magnitude), and
//!   2. posts installment 1 as a compulsory month-budget expense on NEXT month's
//!      Other bucket.
//!
//! The rollover path is then suppressed for the financed month: when the
//! lifecycle computes the rollover INTO next month, it sees an active deficit
//! obligation whose `origin_month_id` is the closed month and rolls ZERO forward
//! (`MonthLifecycleService::prior_month_net`) — so next month's Other absorbs only
//! installment 1, not the whole deficit. Installments 2..N post in the following
//! months via the existing [`crate::fund::FundService::post_installment`] until
//! `remaining_amount == 0`. The sum of installments equals the deficit exactly
//! (the final installment absorbs the rounding remainder), so the deficit is
//! counted exactly once across the whole chain.

use std::sync::Arc;

use chrono::{DateTime, NaiveDate, Utc};
use rust_decimal::Decimal;

use budget_domain::category::Category;
use budget_domain::enums::{
    ObligationSource, ObligationStatus, TransactionSource, TransactionStatus,
};
use budget_domain::error::DomainError;
use budget_domain::ids::{RepaymentObligationId, TransactionId};
use budget_domain::money::Money;
use budget_domain::month::Month;
use budget_domain::repayment_obligation::RepaymentObligation;
use budget_domain::repositories::{BudgetRepository, FundRepository, TransactionRepository};
use budget_domain::transaction::Transaction;
use budget_domain::uow::{UnitOfWork, UowProvider, UowProviderExt};

use crate::config::DeficitFinancingConfig;
use crate::month_lifecycle::MonthLifecycleService;

/// The offer surfaced when a closed month's deficit is large enough to be
/// electively financed (`SPEC §12` D9). Returned by
/// [`DeficitFinancingService::detect_financeable_deficit`]; carries the figures a
/// UI needs to present the choice. The option is never forced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeficitFinancingOffer {
    /// The closed month's net deficit as a POSITIVE magnitude (the principal that
    /// would be amortized). Only present because the deficit is real (net < 0).
    pub deficit_amount: Money,
    /// The next month's Other (rollover-bucket) budget, against which the
    /// threshold is measured.
    pub next_month_other_budget: Money,
    /// The threshold ratio in force (default `0.75`); the offer exists because
    /// `deficit_amount > threshold * next_month_other_budget`.
    pub threshold: Decimal,
}

/// Deficit-financing use case (`SPEC §12` D9, `BUDGET-DEFICIT-FINANCING-1`).
///
/// Holds `Arc<dyn _>` repository + provider dependencies (`SERVICE-DI-1`); all
/// `db.*` lives in the repositories. Reuses
/// [`MonthLifecycleService::month_net_for`] for the deficit figure (single-source,
/// no drift) and the obligation/installment machinery for the repayment.
pub struct DeficitFinancingService {
    lifecycle: Arc<MonthLifecycleService>,
    budgets: Arc<dyn BudgetRepository>,
    transactions: Arc<dyn TransactionRepository>,
    funds: Arc<dyn FundRepository>,
    uow: Arc<dyn UowProvider>,
    config: DeficitFinancingConfig,
}

impl DeficitFinancingService {
    /// Wire the service from its dependencies (`SERVICE-DI-1`). `config` carries
    /// the threshold ratio (default 75%, `DeficitFinancingConfig::default`).
    #[must_use]
    pub fn new(
        lifecycle: Arc<MonthLifecycleService>,
        budgets: Arc<dyn BudgetRepository>,
        transactions: Arc<dyn TransactionRepository>,
        funds: Arc<dyn FundRepository>,
        uow: Arc<dyn UowProvider>,
        config: DeficitFinancingConfig,
    ) -> Self {
        Self {
            lifecycle,
            budgets,
            transactions,
            funds,
            uow,
            config,
        }
    }

    /// Detect whether a closed month's deficit is large enough to OFFER financing
    /// (`SPEC §12` D9). Returns `Some(offer)` ONLY when the month is in deficit AND
    /// the deficit magnitude `>` `threshold` × the next month's Other budget;
    /// otherwise `None` (a surplus, a zero net, or a sub-threshold deficit — all of
    /// which simply roll forward per `SPEC §4.3`).
    ///
    /// The comparison is strict (`>`), so a deficit exactly AT the threshold does
    /// NOT trigger the offer (it rolls forward); only a deficit strictly over it
    /// does.
    ///
    /// # Errors
    /// [`DomainError`] if the next month's budget version / rollover bucket cannot
    /// be resolved, or on any persistence failure.
    pub async fn detect_financeable_deficit(
        &self,
        closed_month: &Month,
    ) -> Result<Option<DeficitFinancingOffer>, DomainError> {
        let net = self.lifecycle.month_net_for(closed_month).await?;
        if !net.is_negative() {
            // Surplus or break-even: nothing to finance.
            return Ok(None);
        }
        let deficit_amount = net.abs();
        let next_month_other_budget = self.next_month_other_budget(closed_month).await?;

        // threshold * next-month Other; strict > so an exactly-at-threshold deficit
        // rolls forward rather than triggering the offer.
        let trigger = self.config.threshold_ratio * next_month_other_budget.as_decimal();
        if deficit_amount.as_decimal() > trigger {
            Ok(Some(DeficitFinancingOffer {
                deficit_amount,
                next_month_other_budget,
                threshold: self.config.threshold_ratio,
            }))
        } else {
            Ok(None)
        }
    }

    /// Convert a closed month's deficit into a [`RepaymentObligation`] amortized
    /// over `months` (`SPEC §12` D9, `BUDGET-DEFICIT-FINANCING-1`).
    ///
    /// Atomically (`SERVICE-TX-1`):
    ///   - creates the obligation (`source = deficit`, `origin_month_id = closed
    ///     month`, `transaction_id = None`, principal = the deficit magnitude,
    ///     `installment_amount = principal / months`), and
    ///   - posts installment 1 as a compulsory month-budget expense on the NEXT
    ///     month's Other bucket (`is_fund_draw = false`, so it COUNTS — reducing
    ///     next month's Other by ONLY installment 1).
    ///
    /// The full deficit no longer rolls forward: the lifecycle rollover path sees
    /// this active deficit obligation for the closed month and rolls ZERO forward
    /// ([`MonthLifecycleService::prior_month_net`]). Installments 2..N post in the
    /// following months via [`crate::fund::FundService::post_installment`] until
    /// `remaining_amount == 0`. The installments sum to the deficit exactly (the
    /// final one absorbs the rounding remainder), so the deficit is counted exactly
    /// once across the whole chain.
    ///
    /// `next_month` is the month into which installment 1 is posted (the month
    /// immediately after `closed_month`); `fund_id` is the buffer that anchors the
    /// obligation (`funds.kind = 'buffer'`, untracked — it only anchors the
    /// repayment, `BUDGET-BUFFER-UNTRACKED-1`).
    ///
    /// # Errors
    /// [`DomainError`] if the month is not actually in deficit, `months` is not
    /// positive, the next month's rollover bucket cannot be resolved, or on any
    /// persistence failure.
    pub async fn finance_deficit(
        &self,
        closed_month: &Month,
        next_month: &Month,
        fund_id: budget_domain::ids::FundId,
        months: i32,
        date: NaiveDate,
        now: DateTime<Utc>,
    ) -> Result<RepaymentObligation, DomainError> {
        if months <= 0 {
            return Err(DomainError::Invariant(
                "deficit-financing repayment must span at least one month".to_owned(),
            ));
        }
        let net = self.lifecycle.month_net_for(closed_month).await?;
        if !net.is_negative() {
            return Err(DomainError::Invariant(
                "finance_deficit called on a month that is not in deficit".to_owned(),
            ));
        }
        let principal = net.abs();

        // The next month's Other bucket carries installment 1 (and is the category
        // the suppressed deficit would otherwise have rolled into).
        let bucket = self.next_month_rollover_bucket(closed_month).await?;

        // installment = principal / months, rounded to cents; the final installment
        // absorbs any rounding remainder so the installments sum to the principal
        // EXACTLY (mirrors the buffer-financed-purchase convention, D7).
        let months_u = u32::try_from(months).unwrap_or(1);
        let installment = principal.divide_into(months_u);

        let obligation = RepaymentObligation {
            id: RepaymentObligationId::generate(),
            user_id: closed_month.user_id,
            fund_id,
            // D9: the principal is the accumulated deficit, not a purchase. No single
            // source transaction; the origin is the closed month.
            source: ObligationSource::Deficit,
            transaction_id: None,
            origin_month_id: Some(closed_month.id),
            total_amount: principal,
            remaining_amount: principal,
            installment_amount: installment,
            months_remaining: months,
            status: ObligationStatus::Active,
            created_at: now,
        };

        // Installment 1: the SAME shape as a buffer-repayment installment — a
        // month-budget expense (is_fund_draw=false) on next month's Other that
        // COUNTS, reducing next month's Other by ONLY this installment. Clamp it to
        // the principal if a single-month financing (months == 1) so a one-shot
        // financing posts the whole principal exactly.
        let first_pay = if months == 1 { principal } else { installment };
        let installment_txn = Transaction {
            id: TransactionId::generate(),
            user_id: closed_month.user_id,
            month_id: next_month.id,
            category_id: Some(bucket.id),
            account_id: None,
            date,
            amount: -first_pay,
            description: "Deficit financing installment".to_owned(),
            source: TransactionSource::Manual,
            plaid_transaction_id: None,
            status: TransactionStatus::Settled,
            income_kind: None,
            is_rollover: false,
            is_fund_draw: false,
            matched_transaction_id: None,
            created_at: now,
            updated_at: now,
        };

        // Decrement the obligation by installment 1 up front: creating the
        // obligation AND posting installment 1 is one logical act, so the stored
        // obligation already reflects installment 1 as paid (remaining = principal -
        // first_pay; months_remaining decremented; flips to paid iff months == 1).
        let mut stored = obligation.clone();
        stored.remaining_amount -= first_pay;
        stored.months_remaining = (stored.months_remaining - 1).max(0);
        if stored.remaining_amount.is_zero() {
            stored.status = ObligationStatus::Paid;
            stored.months_remaining = 0;
        }

        let transactions = Arc::clone(&self.transactions);
        let funds = Arc::clone(&self.funds);
        let to_save = stored.clone();
        self.uow
            .run(move |uow: &dyn UnitOfWork| {
                Box::pin(async move {
                    // Obligation first so the lifecycle suppression sees it on any
                    // concurrent rollover read; then installment 1 — one atomic unit.
                    funds.save_obligation(&to_save, Some(uow)).await?;
                    transactions.save(&installment_txn, Some(uow)).await?;
                    Ok(())
                })
            })
            .await?;
        Ok(stored)
    }

    // -----------------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------------

    /// The next month's Other (rollover-bucket) BUDGET amount (`SPEC §4.3`): the
    /// `amount` of the rollover bucket on the budget version active on the 1st of
    /// the month immediately after `closed_month`.
    async fn next_month_other_budget(&self, closed_month: &Month) -> Result<Money, DomainError> {
        Ok(self.next_month_rollover_bucket(closed_month).await?.amount)
    }

    /// The rollover-bucket [`Category`] of the budget version active on the 1st of
    /// the month immediately after `closed_month`.
    async fn next_month_rollover_bucket(
        &self,
        closed_month: &Month,
    ) -> Result<Category, DomainError> {
        let (ny, nm) = next_month(closed_month.year, closed_month.month);
        let first_of_next = first_of_month(ny, nm)?;
        let budget = self
            .budgets
            .find_active_for_date(closed_month.user_id, first_of_next)
            .await?
            .ok_or_else(|| {
                DomainError::Invariant(format!(
                    "no budget version active on {first_of_next} for {}",
                    closed_month.user_id
                ))
            })?;
        self.budgets
            .find_rollover_bucket(budget.id)
            .await?
            .ok_or_else(|| {
                DomainError::Invariant(format!(
                    "budget version {} has no rollover bucket",
                    budget.id
                ))
            })
    }
}

/// The `(year, month)` immediately after `(year, month)` (December wraps to
/// January of the next year). Mirrors the lifecycle helper.
const fn next_month(year: i32, month: i32) -> (i32, i32) {
    if month >= 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    }
}

/// The 1st of `(year, month)` as a [`NaiveDate`]; the budget-version resolution
/// date (`SPEC §4.1`). Mirrors the lifecycle helper.
fn first_of_month(year: i32, month: i32) -> Result<NaiveDate, DomainError> {
    let m = u32::try_from(month)
        .ok()
        .filter(|m| (1..=12).contains(m))
        .ok_or_else(|| DomainError::IllegalState(format!("month {month} out of range")))?;
    NaiveDate::from_ymd_opt(year, m, 1)
        .ok_or_else(|| DomainError::IllegalState(format!("invalid date {year}-{month:02}-01")))
}

#[cfg(test)]
mod tests;
