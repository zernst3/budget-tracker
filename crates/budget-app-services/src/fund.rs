//! The fund service — the virtual-envelope primitive in three kinds, plus
//! large-purchase resolution and sinking-fund accrual (`SPEC §4.7`, `§4.9`,
//! build step 5; D6 / D7).
//!
//! A fund is ONE primitive with three kinds:
//!   - **buffer** (`compulsory_repayment = true`, has a lean `target_balance`):
//!     the tappable emergency/working pool; a draw that finances a large purchase
//!     creates a [`budget_domain::repayment_obligation::RepaymentObligation`].
//!   - **surplus** (`compulsory_repayment = false`): pre-saved toward a planned
//!     purchase; a draw is a fund-draw, not a re-charged budget expense.
//!   - **sinking** (`SPEC §4.7`): a category-attached virtual envelope
//!     (`Category::fund_balance`) that auto-accrues toward a scheduled recurring
//!     bill and resets on payment.
//!
//! ## The two invariants this service is built around
//!
//! - **`BUDGET-FUND-EARMARK-1` (D6 Model A).** Money moved INTO a fund (sinking
//!   accrual, surplus contribution, buffer-repayment installment) is a manual
//!   Other-bucket expense that **COUNTS** in budget math: it reduces the month
//!   net-leftover (and thus the rolling Other) by the contribution while the fund
//!   balance rises by the same amount. The earmark bites exactly once, through that
//!   Other expense; fund balances are NOT separately subtracted from free-to-spend.
//!   A $50 contribution in an otherwise-zero-net month makes the rolling Other −$50
//!   with the fund balance +$50. Contributions carry `is_fund_draw = false`, so
//!   [`budget_domain::predicates::counts_in_month_expense_remaining`] counts them.
//! - **`BUDGET-NO-DOUBLE-CHARGE-1` (D6 Model A / D7).** A fund DRAW (sinking
//!   payout, surplus draw, buffer financing) is a fund-draw, never a re-charged
//!   budget expense — the money was already expensed at contribution time.
//!   For a buffer-financed purchase the full-price transaction posts for TRACKING
//!   only, with ZERO month-budget impact — excluded from the month
//!   expense-remaining because it is referenced by a repayment obligation (the
//!   buffer fronted the cash). The budget effect is the compulsory installments
//!   flowing back into the buffer until `remaining = 0` -> `status = paid`.
//!
//! ## Buffer health (`SPEC §4.9`)
//!
//! [`FundService::buffer_health`] is an ADVISORY flag only: above target -> excess
//! to invest externally; below target with outstanding obligations -> caution. It
//! NEVER blocks; Zach's judgment, with the data surfaced, decides.
//!
//! ## Transactionality (`SERVICE-TX-1`, `REPO-10`)
//!
//! Every cross-aggregate write (funds + transactions + `repayment_obligations` +
//! the sinking category) runs through the [`UowProvider`] closure
//! ([`UowProviderExt::run`]), committing atomically. The service holds
//! `Arc<dyn _>` dependencies (`SERVICE-DI-1`); no `db.*` lives here.

use std::sync::Arc;

use chrono::{DateTime, NaiveDate, Utc};

use budget_domain::category::Category;
use budget_domain::enums::{
    FundKind, ObligationSource, ObligationStatus, TransactionSource, TransactionStatus,
};
use budget_domain::error::DomainError;
use budget_domain::fund::Fund;
use budget_domain::ids::{
    CategoryId, FundId, MonthId, RepaymentObligationId, TransactionId, UserId,
};
use budget_domain::money::Money;
use budget_domain::repayment_obligation::RepaymentObligation;
use budget_domain::repositories::{BudgetRepository, FundRepository, TransactionRepository};
use budget_domain::transaction::Transaction;
use budget_domain::uow::{UnitOfWork, UowProvider, UowProviderExt};

/// Advisory buffer-health verdict (`SPEC §4.9`) — surfaced, never enforced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BufferHealth {
    /// Balance is above the lean target: the excess should be invested externally
    /// (Zach dislikes idle cash). Carries the excess amount.
    AboveTarget(Money),
    /// Balance is below target AND there are outstanding repayment obligations:
    /// caution before stacking another large draw. Carries the shortfall.
    BelowTargetWithObligations(Money),
    /// Balance is below target with no outstanding obligations: building back up,
    /// no flag. Carries the shortfall.
    BelowTarget(Money),
    /// At or healthily on target (or no target set): nothing to surface.
    OnTarget,
}

/// How a large purchase is resolved at purchase time (`SPEC §4.9` D7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LargePurchaseResolution {
    /// Ordinary expense — posts a normal budget transaction.
    PayInFull,
    /// Draw a pre-saved surplus fund down; NO repayment. The purchase is a
    /// fund-draw, not a re-charged budget expense (reuses
    /// `BUDGET-NO-DOUBLE-CHARGE-1` + sinking-payout logic).
    PayThroughSurplus(FundId),
    /// Buffer fronts the cash: the full-price transaction posts for TRACKING with
    /// ZERO month-budget impact, offset by a buffer draw, and a repayment
    /// obligation is created. The installments are the month-budget expenses.
    BufferFinanced {
        /// The buffer fund fronting the cash.
        fund_id: FundId,
        /// Number of compulsory monthly installments to repay over.
        months: i32,
    },
}

/// The fund service (`SPEC §4.7`, `§4.9`).
///
/// Holds `Arc<dyn _>` repository + provider dependencies (`SERVICE-DI-1`); all
/// `db.*` lives in the repositories.
pub struct FundService {
    funds: Arc<dyn FundRepository>,
    transactions: Arc<dyn TransactionRepository>,
    budgets: Arc<dyn BudgetRepository>,
    uow: Arc<dyn UowProvider>,
}

impl FundService {
    /// Wire the service from its dependencies (`SERVICE-DI-1`).
    #[must_use]
    pub fn new(
        funds: Arc<dyn FundRepository>,
        transactions: Arc<dyn TransactionRepository>,
        budgets: Arc<dyn BudgetRepository>,
        uow: Arc<dyn UowProvider>,
    ) -> Self {
        Self {
            funds,
            transactions,
            budgets,
            uow,
        }
    }

    /// The fund repository handle (`SERVICE-DI-1`) — exposed so an orchestrating
    /// service (e.g. the triage flow) can persist a [`Fund`] /
    /// [`RepaymentObligation`] that [`Self::prepare_existing_fund_draw`] /
    /// [`Self::prepare_existing_buffer_finance`] built, inside ITS own unit of work.
    /// The money math stays here; only the persistence handle is shared.
    #[must_use]
    pub fn fund_repo(&self) -> Arc<dyn FundRepository> {
        Arc::clone(&self.funds)
    }

    // -----------------------------------------------------------------------
    // 1. Fund contributions (BUDGET-FUND-EARMARK-1 / D6)
    // -----------------------------------------------------------------------

    /// Contribute `amount` (a positive magnitude) INTO a buffer or surplus fund
    /// from a month's spendable budget (`BUDGET-FUND-EARMARK-1` / D6 Model A).
    ///
    /// Atomically (`SERVICE-TX-1`):
    ///   - posts a manual EXPENSE transaction for `-amount` against
    ///     `earmark_category_id` (the rollover "Other" bucket) with
    ///     `is_fund_draw = false`, so
    ///     [`budget_domain::predicates::counts_in_month_expense_remaining`]
    ///     **counts** it: the contribution reduces the month net-leftover (and thus
    ///     the rolling Other that carries forward) by `amount`, and
    ///   - increments the fund balance by `amount`.
    ///
    /// Under D6 Model A the earmark bites exactly once, through this Other expense;
    /// fund balances are NOT separately subtracted from free-to-spend. A $50
    /// contribution in an otherwise-zero-net month makes the rolling Other −$50 with
    /// the fund balance +$50.
    ///
    /// # Errors
    /// [`DomainError`] if the fund is absent, `amount` is not positive, or on any
    /// persistence failure.
    pub async fn contribute(
        &self,
        fund_id: FundId,
        month_id: MonthId,
        earmark_category_id: CategoryId,
        amount: Money,
        date: NaiveDate,
        now: DateTime<Utc>,
    ) -> Result<Fund, DomainError> {
        if !amount.is_positive() {
            return Err(DomainError::Invariant(
                "fund contribution amount must be positive".to_owned(),
            ));
        }
        let mut fund = self.load_fund(fund_id).await?;

        // The contribution is a manual EXPENSE against the month (negative amount)
        // on the rollover "Other" bucket; is_fund_draw=false so it COUNTS in the
        // rolling-Other net (BUDGET-FUND-EARMARK-1 / D6 Model A), reducing it by the
        // contribution while the fund balance rises by the same amount.
        let txn = Transaction {
            id: TransactionId::generate(),
            user_id: fund.user_id,
            month_id,
            category_id: Some(earmark_category_id),
            account_id: None,
            date,
            amount: -amount,
            description: format!("Contribution to fund {}", fund.name),
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
        };
        fund.balance += amount;

        let transactions = Arc::clone(&self.transactions);
        let funds = Arc::clone(&self.funds);
        let saved = fund.clone();
        self.uow
            .run(move |uow: &dyn UnitOfWork| {
                Box::pin(async move {
                    transactions.save(&txn, Some(uow)).await?;
                    funds.save(&saved, Some(uow)).await?;
                    Ok(())
                })
            })
            .await?;
        Ok(fund)
    }

    // -----------------------------------------------------------------------
    // 2. Large-purchase resolution (D7)
    // -----------------------------------------------------------------------

    /// Record a large purchase, resolving it one of three ways at purchase time
    /// (`SPEC §4.9` D7).
    ///
    /// `price` is the positive purchase magnitude. The resolution decides the
    /// bookkeeping:
    ///   - [`LargePurchaseResolution::PayInFull`] — an ordinary `-price` expense.
    ///   - [`LargePurchaseResolution::PayThroughSurplus`] — draws the surplus fund
    ///     down; the purchase transaction is a fund-draw assigned to
    ///     `earmark_category_id` (excluded from the net, reusing the sinking-payout
    ///     exclusion + `BUDGET-NO-DOUBLE-CHARGE-1`); NO repayment.
    ///   - [`LargePurchaseResolution::BufferFinanced`] — the full-price `-price`
    ///     transaction posts for TRACKING with ZERO month-budget impact (excluded
    ///     because it is referenced by the repayment obligation we create), the
    ///     buffer is drawn down to front the cash, and an obligation with
    ///     `installment_amount` x `months_remaining` is created
    ///     (`BUDGET-NO-DOUBLE-CHARGE-1`).
    ///
    /// Returns the created purchase transaction id (so callers can show the
    /// tracking row / wire it into the obligation).
    ///
    /// # Errors
    /// [`DomainError`] on a missing/wrong-kind fund, a non-positive price or month
    /// count, or any persistence failure.
    #[allow(clippy::too_many_arguments)]
    pub async fn record_large_purchase(
        &self,
        user_id: UserId,
        month_id: MonthId,
        earmark_category_id: CategoryId,
        price: Money,
        description: String,
        date: NaiveDate,
        resolution: LargePurchaseResolution,
        now: DateTime<Utc>,
    ) -> Result<TransactionId, DomainError> {
        if !price.is_positive() {
            return Err(DomainError::Invariant(
                "large-purchase price must be positive".to_owned(),
            ));
        }
        match resolution {
            LargePurchaseResolution::PayInFull => {
                // An ordinary expense — it COUNTS in the month budget (not a draw).
                let txn = purchase_txn(
                    user_id,
                    month_id,
                    Some(earmark_category_id),
                    price,
                    description,
                    date,
                    false,
                    now,
                );
                let id = txn.id;
                let transactions = Arc::clone(&self.transactions);
                self.uow
                    .run(move |uow: &dyn UnitOfWork| {
                        Box::pin(async move {
                            transactions.save(&txn, Some(uow)).await?;
                            Ok(())
                        })
                    })
                    .await?;
                Ok(id)
            }
            LargePurchaseResolution::PayThroughSurplus(fund_id) => {
                self.draw_through_surplus(
                    user_id,
                    fund_id,
                    month_id,
                    earmark_category_id,
                    price,
                    description,
                    date,
                    now,
                )
                .await
            }
            LargePurchaseResolution::BufferFinanced { fund_id, months } => {
                self.buffer_finance(
                    user_id,
                    fund_id,
                    month_id,
                    price,
                    description,
                    date,
                    months,
                    now,
                )
                .await
            }
        }
    }

    /// Draw a surplus fund down to cover a pre-saved purchase (`SPEC §4.9`): the
    /// purchase posts as a fund-draw on the fund-bound category (excluded from the
    /// net) and the fund balance is reduced; NO repayment.
    #[allow(clippy::too_many_arguments)]
    async fn draw_through_surplus(
        &self,
        user_id: UserId,
        fund_id: FundId,
        month_id: MonthId,
        earmark_category_id: CategoryId,
        price: Money,
        description: String,
        date: NaiveDate,
        now: DateTime<Utc>,
    ) -> Result<TransactionId, DomainError> {
        let mut fund = self.load_fund(fund_id).await?;
        if fund.compulsory_repayment {
            return Err(DomainError::Invariant(
                "pay_through_surplus requires a surplus fund (compulsory_repayment=false)"
                    .to_owned(),
            ));
        }
        // The purchase is a fund-DRAW (is_fund_draw=true): excluded from the
        // rolling-Other net (BUDGET-NO-DOUBLE-CHARGE-1 / D6 Model A) — the money was
        // already expensed by the earlier surplus CONTRIBUTIONS (which count under
        // Model A), so this draw is NOT a re-charged budget expense.
        let txn = purchase_txn(
            user_id,
            month_id,
            Some(earmark_category_id),
            price,
            description,
            date,
            true,
            now,
        );
        let id = txn.id;
        fund.balance -= price;

        let transactions = Arc::clone(&self.transactions);
        let funds = Arc::clone(&self.funds);
        self.uow
            .run(move |uow: &dyn UnitOfWork| {
                Box::pin(async move {
                    transactions.save(&txn, Some(uow)).await?;
                    funds.save(&fund, Some(uow)).await?;
                    Ok(())
                })
            })
            .await?;
        Ok(id)
    }

    /// Buffer-finance a large purchase (`SPEC §4.9` D7): the full-price
    /// transaction posts for TRACKING with zero month-budget impact, the buffer is
    /// drawn down to front the cash, and a repayment obligation is created so the
    /// compulsory installments flow back into the buffer.
    #[allow(clippy::too_many_arguments)]
    async fn buffer_finance(
        &self,
        user_id: UserId,
        fund_id: FundId,
        month_id: MonthId,
        price: Money,
        description: String,
        date: NaiveDate,
        months: i32,
        now: DateTime<Utc>,
    ) -> Result<TransactionId, DomainError> {
        if months <= 0 {
            return Err(DomainError::Invariant(
                "buffer-financed repayment must span at least one month".to_owned(),
            ));
        }
        let mut fund = self.load_fund(fund_id).await?;
        if !fund.compulsory_repayment {
            return Err(DomainError::Invariant(
                "buffer_financed requires a buffer fund (compulsory_repayment=true)".to_owned(),
            ));
        }

        // The full-price tracking transaction: uncategorized so it never lands in
        // a category bucket, and excluded from the month expense-remaining because
        // it is referenced by the obligation below (the buffer fronted the cash).
        // SPEC §4.9 D7 — this is exactly what stops the full price from blowing up
        // its month.
        // is_fund_draw=false: the full-price tracking row is excluded from the month
        // budget via its obligation-keyed list (D7), NOT the fund-draw flag.
        let txn = purchase_txn(
            user_id,
            month_id,
            None,
            price,
            description,
            date,
            false,
            now,
        );
        let txn_id = txn.id;

        // The buffer fronts the cash: draw it down now. The installments restore
        // it back to target.
        fund.balance -= price;

        // SPEC §4.9 D7: installment = price / months, rounded to cents; the final
        // installment absorbs any rounding remainder so the sum is exact.
        let months_u = u32::try_from(months).unwrap_or(1);
        let installment = price.divide_into(months_u);

        let obligation = RepaymentObligation {
            id: RepaymentObligationId::generate(),
            user_id,
            fund_id,
            // D7: a buffer-financed purchase. The single source transaction is the
            // full-price tracking row; no origin month (that is the deficit path, D9).
            source: ObligationSource::LargePurchase,
            transaction_id: Some(txn_id),
            origin_month_id: None,
            total_amount: price,
            remaining_amount: price,
            installment_amount: installment,
            months_remaining: months,
            status: ObligationStatus::Active,
            created_at: now,
        };

        let transactions = Arc::clone(&self.transactions);
        let funds = Arc::clone(&self.funds);
        self.uow
            .run(move |uow: &dyn UnitOfWork| {
                Box::pin(async move {
                    // Insert the tracking txn FIRST so the obligation's
                    // transaction_id FK is satisfiable, then draw the buffer, then
                    // create the obligation — one atomic unit (SERVICE-TX-1).
                    transactions.save(&txn, Some(uow)).await?;
                    funds.save(&fund, Some(uow)).await?;
                    funds.save_obligation(&obligation, Some(uow)).await?;
                    Ok(())
                })
            })
            .await?;
        Ok(txn_id)
    }

    /// Post one compulsory buffer-repayment installment for `obligation_id`
    /// (`SPEC §4.9` D7).
    ///
    /// Atomically (`SERVICE-TX-1`):
    ///   - posts the installment as a month-budget expense on `earmark_category_id`
    ///     (the rollover "Other" bucket) with `is_fund_draw = false`, so it COUNTS
    ///     in the rolling-Other net (`BUDGET-FUND-EARMARK-1` / D6 Model A: money
    ///     flowing back INTO the buffer is a contribution that reduces the net,
    ///     counted once),
    ///   - restores the buffer balance by the installment, and
    ///   - decrements the obligation's remaining + months; when `remaining`
    ///     reaches zero the obligation flips to `paid`.
    ///
    /// The last installment is clamped to the exact remaining amount so rounding
    /// never leaves a residual cent or overshoots the total.
    ///
    /// # Errors
    /// [`DomainError`] if the obligation is absent or already paid, or on any
    /// persistence failure.
    pub async fn post_installment(
        &self,
        obligation_id: RepaymentObligationId,
        month_id: MonthId,
        earmark_category_id: CategoryId,
        date: NaiveDate,
        now: DateTime<Utc>,
    ) -> Result<RepaymentObligation, DomainError> {
        let mut obligation = self
            .funds
            .find_obligation(obligation_id)
            .await?
            .ok_or_else(|| {
                DomainError::Invariant(format!("obligation {obligation_id} not found"))
            })?;
        if obligation.status == ObligationStatus::Paid || obligation.remaining_amount.is_zero() {
            return Err(DomainError::IllegalState(
                "obligation already fully repaid".to_owned(),
            ));
        }
        let mut fund = self.load_fund(obligation.fund_id).await?;

        // Clamp the FINAL installment to the exact remaining so the sum of
        // installments equals the total to the cent: the rounding remainder lands
        // on the last payment (e.g. $100/3 = $33.33 x 2 + $33.34). "Final" is the
        // last scheduled month (`months_remaining <= 1`) or any month where the
        // flat installment would meet/exceed what is left.
        let is_final = obligation.months_remaining <= 1
            || obligation.installment_amount.as_decimal()
                >= obligation.remaining_amount.as_decimal();
        let pay = if is_final {
            obligation.remaining_amount
        } else {
            obligation.installment_amount
        };

        // The installment is a month-budget EXPENSE flowing back into the buffer:
        // negative amount, is_fund_draw=false -> COUNTS in the rolling-Other net
        // (BUDGET-FUND-EARMARK-1 / D6 Model A), reducing free-to-spend this month.
        let txn = Transaction {
            id: TransactionId::generate(),
            user_id: obligation.user_id,
            month_id,
            category_id: Some(earmark_category_id),
            account_id: None,
            date,
            amount: -pay,
            description: "Buffer repayment installment".to_owned(),
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
        };

        fund.balance += pay;
        obligation.remaining_amount -= pay;
        obligation.months_remaining = (obligation.months_remaining - 1).max(0);
        if obligation.remaining_amount.is_zero() {
            obligation.status = ObligationStatus::Paid;
            obligation.months_remaining = 0;
        }

        let transactions = Arc::clone(&self.transactions);
        let funds = Arc::clone(&self.funds);
        let saved_fund = fund.clone();
        let saved_obligation = obligation.clone();
        self.uow
            .run(move |uow: &dyn UnitOfWork| {
                Box::pin(async move {
                    transactions.save(&txn, Some(uow)).await?;
                    funds.save(&saved_fund, Some(uow)).await?;
                    funds.save_obligation(&saved_obligation, Some(uow)).await?;
                    Ok(())
                })
            })
            .await?;
        Ok(obligation)
    }

    // -----------------------------------------------------------------------
    // 3. Sinking funds (SPEC §4.7)
    // -----------------------------------------------------------------------

    /// Accrue one month's reserve into a sinking-fund category's virtual envelope
    /// (`SPEC §4.7`).
    ///
    /// The monthly accrual is `amount / period_months`
    /// ([`Category::accrual_per_month`]). It is:
    ///   - added to the carried-over `Category::fund_balance` (the envelope
    ///     carries forward month to month, unlike a normal category that resets),
    ///     and
    ///   - posted as a manual expense transaction (`is_fund_draw = false`) on the
    ///     sinking category so it reduces free-to-spend AND COUNTS in the
    ///     rolling-Other net (`BUDGET-FUND-EARMARK-1` / D6 Model A): the accrual
    ///     reduces the rolling Other while `fund_balance` rises by the same amount.
    ///
    /// Atomically (`SERVICE-TX-1`): the category balance bump and the contribution
    /// transaction commit together.
    ///
    /// # Errors
    /// [`DomainError`] if the category is not a sinking fund or on any persistence
    /// failure.
    pub async fn accrue_sinking_fund(
        &self,
        category_id: CategoryId,
        month_id: MonthId,
        user_id: UserId,
        date: NaiveDate,
        now: DateTime<Utc>,
    ) -> Result<Category, DomainError> {
        let mut category = self.load_category(category_id).await?;
        if !category.is_sinking_fund() {
            return Err(DomainError::Invariant(format!(
                "category {category_id} is not a sinking fund; cannot accrue"
            )));
        }
        let accrual = category.accrual_per_month();
        if !accrual.is_positive() {
            // Nothing to accrue (zero-budget sinking fund); no-op rather than a
            // zero-amount transaction.
            return Ok(category);
        }
        category.fund_balance += accrual;

        let txn = Transaction {
            id: TransactionId::generate(),
            user_id,
            month_id,
            category_id: Some(category_id),
            account_id: None,
            date,
            amount: -accrual,
            description: format!("Sinking-fund accrual: {}", category.name),
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
        };

        let transactions = Arc::clone(&self.transactions);
        let budgets = Arc::clone(&self.budgets);
        let saved_category = category.clone();
        self.uow
            .run(move |uow: &dyn UnitOfWork| {
                Box::pin(async move {
                    transactions.save(&txn, Some(uow)).await?;
                    budgets.save_category(&saved_category, Some(uow)).await?;
                    Ok(())
                })
            })
            .await?;
        Ok(category)
    }

    /// Tag a real bill as the sinking-fund payout (`SPEC §4.7` reset-on-payment).
    ///
    /// When the periodic bill lands, the user tags that transaction as the payout.
    /// This:
    ///   - draws the reserve (`Category::fund_balance`) down by `paid_amount`, and
    ///   - **resets the accrual clock forward**: `next_due_date` advances from the
    ///     payment date toward the next occurrence (one `effective_period_months`
    ///     ahead) so accrual is forward-looking — you save for the FUTURE
    ///     occurrence, not the past one.
    ///
    /// The payout transaction itself is assigned to the sinking category AND marked
    /// `is_fund_draw = true`, so it is excluded from the rolling-Other net
    /// (`BUDGET-NO-DOUBLE-CHARGE-1` / D6 Model A: it is a fund-draw, not a
    /// re-charged budget expense — the money was already expensed by the accrual
    /// contributions that built the reserve).
    ///
    /// `paid_amount` is the positive bill magnitude; the reserve may go negative if
    /// the bill exceeds what was accrued (under-saved), which surfaces as a
    /// shortfall the next accrual catches up.
    ///
    /// # Errors
    /// [`DomainError`] if the category is not a sinking fund, the payout
    /// transaction is absent / mismatched, or on any persistence failure.
    pub async fn tag_sinking_payout(
        &self,
        category_id: CategoryId,
        payout_transaction_id: TransactionId,
        paid_amount: Money,
        payment_date: NaiveDate,
        now: DateTime<Utc>,
    ) -> Result<Category, DomainError> {
        if !paid_amount.is_positive() {
            return Err(DomainError::Invariant(
                "sinking payout amount must be positive".to_owned(),
            ));
        }
        let mut category = self.load_category(category_id).await?;
        if !category.is_sinking_fund() {
            return Err(DomainError::Invariant(format!(
                "category {category_id} is not a sinking fund; cannot tag a payout"
            )));
        }

        let mut payout = self
            .transactions
            .find_by_id(payout_transaction_id)
            .await?
            .ok_or_else(|| {
                DomainError::Invariant(format!(
                    "payout transaction {payout_transaction_id} not found"
                ))
            })?;
        // Assign the real bill to the sinking category and mark it a fund DRAW so it
        // is excluded from the rolling-Other net (D6 Model A: the reserve — built
        // from already-counted accrual contributions — covers it, not this month's
        // cash; BUDGET-NO-DOUBLE-CHARGE-1).
        payout.category_id = Some(category_id);
        payout.is_fund_draw = true;
        payout.updated_at = now;

        // Draw the reserve down.
        category.fund_balance -= paid_amount;
        // Reset-on-payment: re-anchor the next occurrence forward from the payment
        // date by one full period, so accrual now targets the NEXT bill.
        category.next_due_date = Some(advance_months(
            payment_date,
            category.effective_period_months(),
        ));

        let transactions = Arc::clone(&self.transactions);
        let budgets = Arc::clone(&self.budgets);
        let saved_category = category.clone();
        self.uow
            .run(move |uow: &dyn UnitOfWork| {
                Box::pin(async move {
                    transactions.save(&payout, Some(uow)).await?;
                    budgets.save_category(&saved_category, Some(uow)).await?;
                    Ok(())
                })
            })
            .await?;
        Ok(category)
    }

    // -----------------------------------------------------------------------
    // 4. Triage treatments — apply a §4.9 path to an EXISTING settled
    //    transaction WITHIN a caller-provided unit of work (SPEC §7).
    //
    //    These are the money-logic core the pending-triage flow reuses
    //    (BACKEND-3): the triaged row already exists (a settled Plaid charge),
    //    so unlike the create-path large-purchase methods above these MUTATE the
    //    existing transaction rather than minting a new one — but the fund
    //    arithmetic + obligation machinery are IDENTICAL (no duplicated money
    //    math). Each takes the caller's `&dyn UnitOfWork` so the category/comment
    //    edit and the treatment commit ATOMICALLY in ONE transaction
    //    (SERVICE-TX-1) driven by the triage service.
    // -----------------------------------------------------------------------

    /// Treatment (a) money math: turn an existing settled transaction into a FUND
    /// DRAW from a savings/surplus fund (`SPEC §7` / `§4.9`).
    ///
    /// Loads + validates the fund (DomainError-fallible), then mutates `txn` to a
    /// fund draw (`is_fund_draw = true` — EXCLUDED from the rolling-Other net,
    /// `BUDGET-NO-DOUBLE-CHARGE-1` / D6 Model A: the money was already expensed by
    /// the prior contributions that built the fund) and returns the
    /// balance-decremented [`Fund`]. Persistence is the caller's job (the triage
    /// service enlists both `txn` and the returned fund in its unit of work,
    /// `SERVICE-TX-1`). This is the SAME draw arithmetic as
    /// [`Self::draw_through_surplus`] / [`Self::tag_sinking_payout`], factored so the
    /// triage flow reuses it without duplicating the money math.
    ///
    /// # Errors
    /// [`DomainError`] if the fund is absent or on any persistence read failure.
    pub async fn prepare_existing_fund_draw(
        &self,
        fund_id: FundId,
        txn: &mut Transaction,
        now: DateTime<Utc>,
    ) -> Result<Fund, DomainError> {
        let mut fund = self.load_fund(fund_id).await?;
        // The earmarked dollars were already counted when contributed; this draw is
        // not re-charged against the month (BUDGET-NO-DOUBLE-CHARGE-1).
        txn.is_fund_draw = true;
        txn.updated_at = now;
        fund.balance -= txn.amount.abs();
        Ok(fund)
    }

    /// Treatment (b) money math: turn an existing settled transaction into a
    /// BUFFER-FINANCED purchase amortized over `months` (`SPEC §7` / `§4.9` D7).
    ///
    /// Loads + validates the buffer fund (DomainError-fallible), draws it down by
    /// the full price to front the cash, leaves `txn` as the full-price TRACKING row
    /// (`is_fund_draw = false`, excluded from the month budget via its obligation,
    /// D7 — zero net month impact), and builds a [`RepaymentObligation`] with
    /// `installment_amount = price / months` (final installment absorbs the rounding
    /// remainder so the sum is exact — same as [`Self::buffer_finance`]). Returns the
    /// balance-decremented fund and the obligation; persistence is the caller's job
    /// (enlisted in the triage unit of work, `SERVICE-TX-1`). The money math is NOT
    /// duplicated — it is the same as the buffer-financed large-purchase path.
    ///
    /// # Errors
    /// [`DomainError`] if the fund is absent, is not a buffer
    /// (`compulsory_repayment = false`), `months` is not positive, or on any
    /// persistence read failure.
    pub async fn prepare_existing_buffer_finance(
        &self,
        fund_id: FundId,
        txn: &mut Transaction,
        months: i32,
        now: DateTime<Utc>,
    ) -> Result<(Fund, RepaymentObligation), DomainError> {
        if months <= 0 {
            return Err(DomainError::Invariant(
                "buffer-financed repayment must span at least one month".to_owned(),
            ));
        }
        let mut fund = self.load_fund(fund_id).await?;
        if !fund.compulsory_repayment {
            return Err(DomainError::Invariant(
                "spread_over_months requires a buffer fund (compulsory_repayment=true)".to_owned(),
            ));
        }
        let price = txn.amount.abs();
        // The buffer fronts the cash now; the installments restore it (D7). The
        // tracking row stays is_fund_draw=false and is excluded via its obligation.
        fund.balance -= price;
        txn.updated_at = now;

        // installment = price / months; the final installment absorbs the rounding
        // remainder so the sum is exact (mirrors buffer_finance, D7).
        let months_u = u32::try_from(months).unwrap_or(1);
        let installment = price.divide_into(months_u);

        let obligation = RepaymentObligation {
            id: RepaymentObligationId::generate(),
            user_id: txn.user_id,
            fund_id,
            source: ObligationSource::LargePurchase,
            transaction_id: Some(txn.id),
            origin_month_id: None,
            total_amount: price,
            remaining_amount: price,
            installment_amount: installment,
            months_remaining: months,
            status: ObligationStatus::Active,
            created_at: now,
        };
        Ok((fund, obligation))
    }

    // -----------------------------------------------------------------------
    // 5. Buffer health — advisory only (SPEC §4.9)
    // -----------------------------------------------------------------------

    /// Compute the advisory buffer-health verdict for `fund` (`SPEC §4.9`).
    ///
    /// This is a judgment AID, never enforcement: above target -> excess to invest
    /// externally; below target with outstanding obligations -> caution before
    /// stacking another large draw. It NEVER blocks — callers surface it for
    /// Zach's judgment and do nothing else.
    ///
    /// `has_outstanding_obligations` is the caller's read of
    /// [`FundRepository::list_active_obligations`] for the user (passed in so this
    /// stays a pure function over already-fetched data).
    ///
    /// Returns [`BufferHealth::OnTarget`] for any non-buffer fund or a buffer with
    /// no target set — there is nothing to flag.
    #[must_use]
    pub fn buffer_health(fund: &Fund, has_outstanding_obligations: bool) -> BufferHealth {
        if fund.kind != FundKind::Buffer {
            return BufferHealth::OnTarget;
        }
        let Some(target) = fund.target_balance else {
            return BufferHealth::OnTarget;
        };
        match fund.balance.cmp(&target) {
            std::cmp::Ordering::Greater => BufferHealth::AboveTarget(fund.balance - target),
            std::cmp::Ordering::Less => {
                let shortfall = target - fund.balance;
                if has_outstanding_obligations {
                    BufferHealth::BelowTargetWithObligations(shortfall)
                } else {
                    BufferHealth::BelowTarget(shortfall)
                }
            }
            std::cmp::Ordering::Equal => BufferHealth::OnTarget,
        }
    }

    /// Fetch the advisory buffer-health verdict for a fund, reading the user's
    /// active obligations to decide the below-target branch (`SPEC §4.9`).
    ///
    /// A thin async wrapper over the pure [`Self::buffer_health`] that does the
    /// obligation read; never blocks anything.
    ///
    /// # Errors
    /// [`DomainError`] if the fund is absent or on any persistence failure.
    pub async fn buffer_health_for(&self, fund_id: FundId) -> Result<BufferHealth, DomainError> {
        let fund = self.load_fund(fund_id).await?;
        let has_obligations = !self
            .funds
            .list_active_obligations(fund.user_id)
            .await?
            .is_empty();
        Ok(Self::buffer_health(&fund, has_obligations))
    }

    // -----------------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------------

    async fn load_fund(&self, fund_id: FundId) -> Result<Fund, DomainError> {
        self.funds
            .find_by_id(fund_id)
            .await?
            .ok_or_else(|| DomainError::Invariant(format!("fund {fund_id} not found")))
    }

    async fn load_category(&self, category_id: CategoryId) -> Result<Category, DomainError> {
        self.budgets
            .find_category(category_id)
            .await?
            .ok_or_else(|| DomainError::Invariant(format!("category {category_id} not found")))
    }
}

/// Build a `-price` expense transaction for a large purchase. `category_id`
/// `None` is the uncategorized buffer-financed tracking row; `Some` is the
/// pay-in-full / surplus-draw category assignment.
///
/// `is_fund_draw` marks the row as a fund DRAW that must NOT be re-charged against
/// the month budget (`BUDGET-NO-DOUBLE-CHARGE-1` / D6 Model A): `true` for a
/// surplus draw (money already expensed at contribution time), `false` for a
/// pay-in-full ordinary expense and for the buffer-financed full-price tracking
/// row (the latter is excluded via its obligation-keyed list instead, `SPEC §4.9`
/// D7).
#[allow(clippy::too_many_arguments)]
fn purchase_txn(
    user_id: UserId,
    month_id: MonthId,
    category_id: Option<CategoryId>,
    price: Money,
    description: String,
    date: NaiveDate,
    is_fund_draw: bool,
    now: DateTime<Utc>,
) -> Transaction {
    Transaction {
        id: TransactionId::generate(),
        user_id,
        month_id,
        category_id,
        account_id: None,
        date,
        amount: -price,
        description,
        source: TransactionSource::Manual,
        plaid_transaction_id: None,
        status: TransactionStatus::Settled,
        income_kind: None,
        is_rollover: false,
        is_fund_draw,
        matched_transaction_id: None,
        comment: None,
        is_transfer: false,
        plaid_category: None,
        created_at: now,
        updated_at: now,
    }
}

/// Advance `date` forward by `months` calendar months, clamping the day to the
/// target month's length (e.g. Jan 31 + 1 month -> Feb 28/29). Used by
/// reset-on-payment to re-anchor a sinking fund's `next_due_date` (`SPEC §4.7`).
#[must_use]
fn advance_months(date: NaiveDate, months: u32) -> NaiveDate {
    // chrono::Months clamps the day to the target month's length (e.g.
    // Jan 31 + 1 month -> Feb 28/29) and handles year wrap; falls back to the
    // unchanged date in the impossible overflow case.
    date.checked_add_months(chrono::Months::new(months))
        .unwrap_or(date)
}

#[cfg(test)]
mod tests;
