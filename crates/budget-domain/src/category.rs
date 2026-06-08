//! The [`Category`] aggregate — a spending bucket within a budget version
//! (`SPEC §4.2`, `§4.7`).
//!
//! Carries the group/settle/cadence typing that drives the budget math, the
//! `is_rollover_bucket` flag (exactly one per budget version,
//! `BUDGET-ROLLOVER-INTEGRITY-1`), and the sinking-fund carryover
//! [`Category::fund_balance`] (the virtual envelope, `SPEC §4.7`). Money fields
//! use [`Money`] (`BUDGET-MONEY-1`).

use chrono::NaiveDate;

use crate::enums::{Cadence, CategoryGrp, SettleType};
use crate::ids::{BudgetId, CategoryId, CategoryKey};
use crate::money::Money;

/// A budget category (bucket).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Category {
    /// Stable identity within this budget version.
    pub id: CategoryId,
    /// Owning budget version.
    pub budget_id: BudgetId,
    /// Stable lineage id across budget versions (`D3`). Cross-version reporting
    /// is deferred to V2; the field exists now so no migration is needed later.
    pub category_key: CategoryKey,
    /// Display name. Free-form, no validation.
    pub name: String,
    /// Monthly budgeted amount. For sinking funds the monthly accrual is
    /// `amount / period_months` (see [`Category::accrual_per_month`]).
    pub amount: Money,
    /// Fixed vs. discretionary (`SPEC §4.2`).
    pub grp: CategoryGrp,
    /// Settle type — only meaningful for fixed categories; `None` for discretionary.
    pub settle_type: Option<SettleType>,
    /// `flexible_set` only: how many real transactions must be assigned before
    /// the category is considered fully settled (`SPEC §4.2`).
    pub expected_bills: Option<i32>,
    /// Exactly ONE category per budget version is the rollover bucket ("Other").
    /// Enforced by a DB partial unique index (`ENTITIES-8`).
    pub is_rollover_bucket: bool,
    /// Accrual cadence. `Monthly` = normal; longer = sinking fund (`SPEC §4.7`).
    pub cadence: Cadence,
    /// Arbitrary cadence override in months; `None` = use the `cadence` enum's
    /// implied period.
    pub period_months: Option<i32>,
    /// Sinking-fund carryover balance — the virtual envelope (`SPEC §4.7`).
    pub fund_balance: Money,
    /// Sinking-fund next occurrence; resets on payment to anchor the next cycle.
    pub next_due_date: Option<NaiveDate>,
    /// Display ordering within the budget version.
    pub sort_order: i32,
}

impl Category {
    /// `true` when this category is a sinking fund (cadence longer than monthly).
    #[must_use]
    pub fn is_sinking_fund(&self) -> bool {
        self.cadence.is_sinking_fund() || self.period_months.is_some_and(|m| m > 1)
    }

    /// The effective accrual period in months: the explicit `period_months`
    /// override when set, otherwise the cadence's implied period (`SPEC §4.7`).
    #[must_use]
    pub fn effective_period_months(&self) -> u32 {
        match self.period_months {
            Some(m) if m > 0 => u32::try_from(m).unwrap_or(1),
            _ => self.cadence.period_months(),
        }
    }

    /// The monthly sinking-fund accrual = `amount / period_months`, rounded to
    /// cents (`SPEC §4.7`). For a monthly category this is just `amount`.
    #[must_use]
    pub fn accrual_per_month(&self) -> Money {
        self.amount.divide_into(self.effective_period_months())
    }
}
