//! The income-expectation seam (`SPEC §4.8`, `D5`).
//!
//! The month net-leftover formula (`D5`, `BUDGET-ROLLOVER-INTEGRITY-1`) is
//!
//! ```text
//! net = (actual_income - expected_income) + Σ(expense category remaining)
//! ```
//!
//! Computing **`expected_income`** for a `(year, month)` is the whole of income
//! step 6 (the full mode matrix: `per_paycheck` vs `smoothed`, semimonthly /
//! biweekly / weekly / hourly cadence resolution, the smoothing buffer, and
//! surplus routing). The month-lifecycle service (this step, build step 4) only
//! needs the *figure* — not the machinery that produces it — so the figure is
//! injected behind the [`IncomeExpectation`] trait. Step 6 fills it; nothing in
//! the lifecycle service changes when it does.
//!
//! ## What this seam deliberately leaves for step 6
//!
//! - All four [`budget_domain::enums::PaycheckType`] cadences except the exact
//!   semimonthly case (which is `2 × amount` every month and needs no anchor
//!   arithmetic — `SPEC §4.8`, "Zach's own situation").
//! - The `smoothed` [`budget_domain::enums::IncomeMode`] and its smoothing
//!   buffer (dormant for the semimonthly case; `SPEC §4.8`).
//! - Surplus routing (`buffer` / `this_month` / `savings`) and the
//!   per-transaction "add to this month" override (`SPEC §4.8`). In
//!   `per_paycheck` mode the surplus auto-raises Other by formula with no
//!   routing decision (`D5`), so the lifecycle service needs none of it now.
//!
//! The minimal [`SemimonthlyFixedExpectation`] implemented here covers exactly
//! the case the netting math must be tested against today (`SPEC §4.8`: Zach is
//! semimonthly, always 2 paychecks/month, both modes identical, buffer never
//! fires).

use budget_domain::ids::UserId;
use budget_domain::money::Money;

/// The expected (budgeted) income for a user in a given calendar month.
///
/// This is the `expected_income` term of the `D5` net formula. It is the single
/// seam between the month-lifecycle service (build step 4) and the income
/// engine (build step 6): the lifecycle service depends on this trait, never on
/// a concrete income mode. Step 6 provides the real, mode-aware implementation
/// (per-paycheck cadence resolution, smoothed averaging, the buffer); until
/// then [`SemimonthlyFixedExpectation`] is sufficient for Zach's case and for
/// testing the netting.
///
/// Month-membership of paychecks is the implementor's concern and is computed in
/// the fixed home timezone (`America/New_York`, `D2`) consistent with the rest
/// of the lifecycle service.
pub trait IncomeExpectation: Send + Sync {
    /// The expected income for `user` in the calendar `(year, month)`.
    ///
    /// Returns a signed [`Money`] in the internal convention (income is
    /// positive). Implementors that cannot form an expectation (hourly /
    /// variable income with a blank amount, `SPEC §4.8`) return
    /// [`Money::ZERO`], which degrades the net to pure actual-income tracking —
    /// `actual - 0 = actual` flows straight into Other.
    fn expected_income(&self, user: UserId, year: i32, month: i32) -> Money;
}

/// The minimal income expectation for the only mode built in V1: semimonthly,
/// fixed per-paycheck (`SPEC §4.8`, "Zach's own situation").
///
/// Semimonthly is **always exactly two paychecks per month**, so the expected
/// income is `2 × amount` every month, independent of the anchor date — no
/// cadence arithmetic, no buffer, both [`budget_domain::enums::IncomeMode`]
/// values identical. That is precisely the case the `D5` netting must be tested
/// against now, and precisely the case step 6 will generalise from.
///
/// Step 6 replaces this with a config-driven implementation that reads the
/// user's [`budget_domain::paycheck_config::PaycheckConfig`] and resolves the
/// other three cadences + the smoothed mode. The lifecycle service does not
/// change when it does — it only ever sees [`IncomeExpectation`].
#[derive(Debug, Clone, Copy)]
pub struct SemimonthlyFixedExpectation {
    per_paycheck_amount: Money,
}

impl SemimonthlyFixedExpectation {
    /// The number of semimonthly paychecks in any month — always two
    /// (`SPEC §4.8`). A named constant so the "2" is not a bare magic number at
    /// the call site.
    const PAYCHECKS_PER_MONTH: i64 = 2;

    /// Build the expectation from the fixed per-paycheck amount.
    #[must_use]
    pub const fn new(per_paycheck_amount: Money) -> Self {
        Self {
            per_paycheck_amount,
        }
    }
}

impl IncomeExpectation for SemimonthlyFixedExpectation {
    fn expected_income(&self, _user: UserId, _year: i32, _month: i32) -> Money {
        // Semimonthly = 2 paychecks every month, every month (SPEC §4.8). The
        // amount is independent of the (year, month) and of the anchor date, so
        // the expectation is a flat 2 × amount. Step 6 generalises this.
        let mut total = Money::ZERO;
        for _ in 0..Self::PAYCHECKS_PER_MONTH {
            total += self.per_paycheck_amount;
        }
        total
    }
}

/// An income expectation that always returns a fixed, injected value.
///
/// Two uses:
///   - the hourly / variable degradation path (`SPEC §4.8`: blank amount ->
///     [`Money::ZERO`] -> pure actual tracking), and
///   - tests that want to drive the netting with an exact expected figure
///     without reconstructing cadence math.
#[derive(Debug, Clone, Copy)]
pub struct FixedExpectation {
    value: Money,
}

impl FixedExpectation {
    /// Build a fixed expectation returning `value` for every month.
    #[must_use]
    pub const fn new(value: Money) -> Self {
        Self { value }
    }

    /// The zero expectation — the hourly / variable degradation default
    /// (`SPEC §4.8`).
    #[must_use]
    pub const fn zero() -> Self {
        Self { value: Money::ZERO }
    }
}

impl IncomeExpectation for FixedExpectation {
    fn expected_income(&self, _user: UserId, _year: i32, _month: i32) -> Money {
        self.value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semimonthly_fixed_is_two_paychecks_regardless_of_month() {
        // SPEC §4.8: semimonthly is always exactly 2 paychecks/month.
        let exp = SemimonthlyFixedExpectation::new(Money::from_major(2_000));
        let user = UserId::generate();
        // Same figure every month of the year.
        for month in 1..=12 {
            assert_eq!(
                exp.expected_income(user, 2026, month),
                Money::from_major(4_000),
                "semimonthly expectation must be 2 x amount in month {month}"
            );
        }
    }

    #[test]
    fn fixed_expectation_returns_injected_value() {
        let exp = FixedExpectation::new(Money::from_minor(123_456));
        assert_eq!(
            exp.expected_income(UserId::generate(), 2026, 6),
            Money::from_minor(123_456)
        );
    }

    #[test]
    fn zero_expectation_degrades_to_actual_tracking() {
        // SPEC §4.8: hourly/variable with a blank amount -> zero expectation, so
        // the net becomes (actual - 0) = actual.
        let exp = FixedExpectation::zero();
        assert_eq!(
            exp.expected_income(UserId::generate(), 2026, 6),
            Money::ZERO
        );
    }
}
