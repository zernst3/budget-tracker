//! The [`Fund`] aggregate — the virtual-envelope primitive (`SPEC §4.9`).
//!
//! Two kinds (`FundKind`):
//!   - `Buffer`: the emergency / working savings pool. `compulsory_repayment =
//!     true`; drawing it to finance a large purchase creates a
//!     [`crate::repayment_obligation::RepaymentObligation`]. Has a lean
//!     [`Fund::target_balance`]; the app flags balances above target (excess to
//!     invest externally) or below target with outstanding obligations.
//!   - `Surplus`: a deliberate pre-saved pool for a planned purchase.
//!     `compulsory_repayment = false`; a draw is a fund-draw, not a re-charged
//!     budget expense (`BUDGET-NO-DOUBLE-CHARGE-1`).
//!
//! `BUDGET-FUND-EARMARK-1`: money moved INTO a fund is an expense against the
//! month and is excluded from the rollover net, so an earmarked dollar is never
//! double-counted. Balances use [`Money`] (`BUDGET-MONEY-1`).
//!
//! Note: sinking-fund carryover lives on [`crate::category::Category::fund_balance`],
//! not as a `Fund` row — sinking funds are category-attached.

use chrono::{DateTime, Utc};

use crate::enums::FundKind;
use crate::ids::{FundId, UserId};
use crate::money::Money;

/// A virtual-envelope fund (buffer or surplus).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fund {
    /// Stable identity.
    pub id: FundId,
    /// Owning user.
    pub user_id: UserId,
    /// Display name. Free-form, no validation.
    pub name: String,
    /// Buffer vs. surplus (`SPEC §4.9`).
    pub kind: FundKind,
    /// Current balance.
    pub balance: Money,
    /// Buffer-only lean target; `None` for surplus funds. The app flags balances
    /// above this (excess to invest) or below it with outstanding obligations.
    pub target_balance: Option<Money>,
    /// `true` for buffer (compulsory repayment); `false` for surplus.
    pub compulsory_repayment: bool,
    /// When the fund was created (UTC, `DOMAIN-7`).
    pub created_at: DateTime<Utc>,
}

impl Fund {
    /// `true` when the balance exceeds the lean target (excess to invest
    /// externally, `SPEC §4.9`). Always `false` when no target is set.
    #[must_use]
    pub fn is_above_target(&self) -> bool {
        self.target_balance.is_some_and(|t| self.balance > t)
    }

    /// `true` when the balance is below the lean target (`SPEC §4.9`). Always
    /// `false` when no target is set.
    #[must_use]
    pub fn is_below_target(&self) -> bool {
        self.target_balance.is_some_and(|t| self.balance < t)
    }
}
