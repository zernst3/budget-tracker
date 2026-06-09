//! The [`RepaymentObligation`] aggregate (`SPEC §4.9` D7).
//!
//! Created when the buffer funds a large purchase ("pay off in X months"). The
//! full-price transaction posts immediately for accurate tracking; the
//! *budget* impact is the compulsory monthly installments flowing back into the
//! buffer until [`RepaymentObligation::remaining_amount`] reaches zero.
//!
//! Two parents, by source ([`RepaymentObligation::source`], `SPEC §12` D9):
//!   - a **large purchase** (`SPEC §4.9` D7) pins [`RepaymentObligation::fund_id`]
//!     (the buffer being repaid) and [`RepaymentObligation::transaction_id`] (the
//!     single purchase transaction);
//!   - a financed **deficit** (`SPEC §12` D9, `BUDGET-DEFICIT-FINANCING-1`) pins
//!     [`RepaymentObligation::origin_month_id`] (the closed month whose
//!     accumulated deficit was financed) and has NO single
//!     [`RepaymentObligation::transaction_id`] (it is `None`).
//!
//! Both reuse the SAME repayment machinery (compulsory monthly installments back
//! into the buffer until `remaining = 0`). All monetary fields use [`Money`]
//! (`BUDGET-MONEY-1`).

use chrono::{DateTime, Utc};

use crate::enums::{ObligationSource, ObligationStatus};
use crate::ids::{FundId, MonthId, RepaymentObligationId, TransactionId, UserId};
use crate::money::Money;

/// A compulsory repayment obligation (buffer-financed purchase OR financed
/// deficit; `SPEC §4.9` D7 / `§12` D9).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepaymentObligation {
    /// Stable identity.
    pub id: RepaymentObligationId,
    /// Owning user.
    pub user_id: UserId,
    /// The buffer fund being repaid.
    pub fund_id: FundId,
    /// What the principal represents (`SPEC §12` D9): a large purchase or a
    /// financed accumulated deficit.
    pub source: ObligationSource,
    /// The large-purchase transaction (marked spent in full at purchase).
    /// `Some` for [`ObligationSource::LargePurchase`]; `None` for
    /// [`ObligationSource::Deficit`] (a deficit has no single source transaction).
    pub transaction_id: Option<TransactionId>,
    /// The closed month whose accumulated deficit was financed (`SPEC §12` D9).
    /// `Some` for [`ObligationSource::Deficit`]; `None` for a large purchase.
    pub origin_month_id: Option<MonthId>,
    /// Full purchase price.
    pub total_amount: Money,
    /// Remaining amount still owed back to the buffer.
    pub remaining_amount: Money,
    /// Compulsory monthly installment.
    pub installment_amount: Money,
    /// Number of installments still to pay.
    pub months_remaining: i32,
    /// Lifecycle status.
    pub status: ObligationStatus,
    /// When the obligation was created (UTC, `DOMAIN-7`).
    pub created_at: DateTime<Utc>,
}

impl RepaymentObligation {
    /// `true` when fully repaid (`remaining_amount` is zero).
    #[must_use]
    pub fn is_settled(&self) -> bool {
        self.remaining_amount.is_zero()
    }
}
