//! The [`RepaymentObligation`] aggregate (`SPEC §4.9` D7).
//!
//! Created when the buffer funds a large purchase ("pay off in X months"). The
//! full-price transaction posts immediately for accurate tracking; the
//! *budget* impact is the compulsory monthly installments flowing back into the
//! buffer until [`RepaymentObligation::remaining_amount`] reaches zero.
//!
//! Two business FKs to different parents: [`RepaymentObligation::fund_id`] (the
//! buffer being repaid) and [`RepaymentObligation::transaction_id`] (the large
//! purchase). All monetary fields use [`Money`] (`BUDGET-MONEY-1`).

use chrono::{DateTime, Utc};

use crate::enums::ObligationStatus;
use crate::ids::{FundId, RepaymentObligationId, TransactionId, UserId};
use crate::money::Money;

/// A compulsory buffer-repayment obligation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepaymentObligation {
    /// Stable identity.
    pub id: RepaymentObligationId,
    /// Owning user.
    pub user_id: UserId,
    /// The buffer fund being repaid.
    pub fund_id: FundId,
    /// The large-purchase transaction (marked spent in full at purchase).
    pub transaction_id: TransactionId,
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
