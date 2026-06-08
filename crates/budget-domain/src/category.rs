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

#[cfg(test)]
mod tests {
    use rust_decimal::Decimal;
    use uuid::Uuid;

    use crate::enums::{Cadence, CategoryGrp, PaycheckType};
    use crate::ids::{BudgetId, CategoryId, CategoryKey};
    use crate::money::Money;

    use super::*;

    // -----------------------------------------------------------------------
    // Cadence helpers
    // -----------------------------------------------------------------------

    #[test]
    fn cadence_period_months_all_variants() {
        assert_eq!(Cadence::Monthly.period_months(), 1);
        assert_eq!(Cadence::Quarterly.period_months(), 3);
        assert_eq!(Cadence::Semiannual.period_months(), 6);
        assert_eq!(Cadence::Annual.period_months(), 12);
    }

    #[test]
    fn cadence_is_sinking_fund() {
        assert!(
            !Cadence::Monthly.is_sinking_fund(),
            "Monthly is not a sinking fund"
        );
        assert!(
            Cadence::Quarterly.is_sinking_fund(),
            "Quarterly is a sinking fund"
        );
        assert!(
            Cadence::Semiannual.is_sinking_fund(),
            "Semiannual is a sinking fund"
        );
        assert!(
            Cadence::Annual.is_sinking_fund(),
            "Annual is a sinking fund"
        );
    }

    // -----------------------------------------------------------------------
    // PaycheckType helpers
    // -----------------------------------------------------------------------

    #[test]
    fn paycheck_type_paychecks_per_year_all_variants() {
        assert_eq!(PaycheckType::Semimonthly.paychecks_per_year(), Some(24));
        assert_eq!(PaycheckType::Biweekly.paychecks_per_year(), Some(26));
        assert_eq!(PaycheckType::Weekly.paychecks_per_year(), Some(52));
        assert_eq!(
            PaycheckType::Hourly.paychecks_per_year(),
            None,
            "Hourly has no annual count"
        );
    }

    // -----------------------------------------------------------------------
    // Category helpers — test fixture
    // -----------------------------------------------------------------------

    fn make_category(cadence: Cadence, period_months: Option<i32>, amount: Decimal) -> Category {
        Category {
            id: CategoryId::new(Uuid::new_v4()),
            budget_id: BudgetId::new(Uuid::new_v4()),
            category_key: CategoryKey::new(Uuid::new_v4()),
            name: "Test".to_owned(),
            amount: Money::from_decimal(amount),
            grp: CategoryGrp::Fixed,
            settle_type: None,
            expected_bills: None,
            is_rollover_bucket: false,
            cadence,
            period_months,
            fund_balance: Money::ZERO,
            next_due_date: None,
            sort_order: 0,
        }
    }

    // -----------------------------------------------------------------------
    // Category::is_sinking_fund
    // -----------------------------------------------------------------------

    #[test]
    fn is_sinking_fund_monthly_no_override_is_false() {
        let cat = make_category(Cadence::Monthly, None, Decimal::new(10000, 2));
        assert!(!cat.is_sinking_fund());
    }

    #[test]
    fn is_sinking_fund_quarterly_cadence_is_true() {
        let cat = make_category(Cadence::Quarterly, None, Decimal::new(30000, 2));
        assert!(cat.is_sinking_fund());
    }

    #[test]
    fn is_sinking_fund_semiannual_cadence_is_true() {
        let cat = make_category(Cadence::Semiannual, None, Decimal::new(60000, 2));
        assert!(cat.is_sinking_fund());
    }

    #[test]
    fn is_sinking_fund_annual_cadence_is_true() {
        let cat = make_category(Cadence::Annual, None, Decimal::new(120_000, 2));
        assert!(cat.is_sinking_fund());
    }

    /// A Monthly cadence with an explicit `period_months` > 1 override is still a
    /// sinking fund — the override governs.
    #[test]
    fn is_sinking_fund_monthly_with_period_override_is_true() {
        let cat = make_category(Cadence::Monthly, Some(4), Decimal::new(40000, 2));
        assert!(cat.is_sinking_fund());
    }

    /// `period_months` = 1 on a Monthly cadence is not a sinking fund.
    #[test]
    fn is_sinking_fund_monthly_with_period_one_is_false() {
        let cat = make_category(Cadence::Monthly, Some(1), Decimal::new(10000, 2));
        assert!(!cat.is_sinking_fund());
    }

    // -----------------------------------------------------------------------
    // Category::effective_period_months
    // -----------------------------------------------------------------------

    /// No override — cadence's implied period is returned.
    #[test]
    fn effective_period_months_uses_cadence_when_no_override() {
        assert_eq!(
            make_category(Cadence::Quarterly, None, Decimal::ZERO).effective_period_months(),
            3
        );
        assert_eq!(
            make_category(Cadence::Semiannual, None, Decimal::ZERO).effective_period_months(),
            6
        );
        assert_eq!(
            make_category(Cadence::Annual, None, Decimal::ZERO).effective_period_months(),
            12
        );
    }

    /// Explicit positive `period_months` overrides the cadence enum.
    #[test]
    fn effective_period_months_positive_override_takes_precedence() {
        // Annual cadence, but 9-month override.
        let cat = make_category(Cadence::Annual, Some(9), Decimal::ZERO);
        assert_eq!(cat.effective_period_months(), 9);
    }

    /// `period_months` = 0 (or negative) falls back to the cadence's implied period.
    #[test]
    fn effective_period_months_zero_override_falls_back_to_cadence() {
        let cat = make_category(Cadence::Quarterly, Some(0), Decimal::ZERO);
        assert_eq!(
            cat.effective_period_months(),
            3,
            "zero override falls back to cadence"
        );
    }

    // -----------------------------------------------------------------------
    // Category::accrual_per_month
    // -----------------------------------------------------------------------

    /// Monthly category: accrual = amount (period = 1, no division).
    #[test]
    fn accrual_per_month_monthly_is_full_amount() {
        let cat = make_category(Cadence::Monthly, None, Decimal::new(50000, 2)); // $500.00
        assert_eq!(
            cat.accrual_per_month(),
            Money::from_decimal(Decimal::new(50000, 2))
        );
    }

    /// Quarterly: $300 / 3 = $100/month.
    #[test]
    fn accrual_per_month_quarterly_divides_by_three() {
        let cat = make_category(Cadence::Quarterly, None, Decimal::new(30000, 2)); // $300.00
        assert_eq!(
            cat.accrual_per_month(),
            Money::from_decimal(Decimal::new(10000, 2)) // $100.00
        );
    }

    /// Semiannual: $600 / 6 = $100/month.
    #[test]
    fn accrual_per_month_semiannual_divides_by_six() {
        let cat = make_category(Cadence::Semiannual, None, Decimal::new(60000, 2)); // $600.00
        assert_eq!(
            cat.accrual_per_month(),
            Money::from_decimal(Decimal::new(10000, 2)) // $100.00
        );
    }

    /// Annual: $1200 / 12 = $100/month.
    #[test]
    fn accrual_per_month_annual_divides_by_twelve() {
        let cat = make_category(Cadence::Annual, None, Decimal::new(120_000, 2)); // $1200.00
        assert_eq!(
            cat.accrual_per_month(),
            Money::from_decimal(Decimal::new(10000, 2)) // $100.00
        );
    }

    /// `period_months` override: Annual cadence with 9-month override; $900 / 9 = $100/month.
    #[test]
    fn accrual_per_month_uses_period_months_override() {
        let cat = make_category(Cadence::Annual, Some(9), Decimal::new(90000, 2)); // $900.00
        assert_eq!(
            cat.accrual_per_month(),
            Money::from_decimal(Decimal::new(10000, 2)) // $100.00
        );
    }

    /// Non-even division: $100 / 3 rounds to cents (banker's rounding).
    #[test]
    fn accrual_per_month_rounds_to_cents() {
        let cat = make_category(Cadence::Quarterly, None, Decimal::new(10000, 2)); // $100.00
        let accrual = cat.accrual_per_month();
        // $100 / 3 = $33.333... → rounds to $33.33 (banker's rounding, round_dp(2))
        assert_eq!(accrual, Money::from_decimal(Decimal::new(3333, 2))); // $33.33
    }
}
