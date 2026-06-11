//! The single [`Money`] type (`BUDGET-MONEY-1` / `DOMAIN-8` / `ARCH-EXACT-DECIMALS-1`).
//!
//! Every monetary amount in the domain — category budgets, transaction amounts,
//! fund balances, rollover nets — uses this one type. It is a newtype around
//! [`rust_decimal::Decimal`], which maps Postgres `NUMERIC` exactly. Floating
//! point (`f32`/`f64`) is FORBIDDEN for any value that represents money: the
//! workspace lints + this single type are the mechanical enforcement.
//!
//! Why a newtype rather than a bare `Decimal`: the rolling-Other balance
//! (`SPEC §4.3`) compounds every month, so a one-cent drift becomes a permanent
//! ledger error. Centralising money in one type means rounding happens at
//! defined points ([`Money::round_to_cents`]) instead of being scattered, and a
//! property test (below) can assert that summing N transactions and rolling the
//! balance forward M months loses zero cents versus a `Decimal` oracle.

use std::iter::Sum;
use std::ops::{Add, AddAssign, Neg, Sub, SubAssign};

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::error::ValidationError;

/// An exact monetary amount, backed by [`rust_decimal::Decimal`].
///
/// Signed: negative is an expense / outflow, positive is income / inflow
/// (the internal convention; `BUDGET-PLAID-SIGN-1` normalises Plaid amounts to
/// it at the mapper boundary). Arithmetic is exact; rounding is explicit via
/// [`Money::round_to_cents`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Money(Decimal);

impl Money {
    /// Zero money. Useful as a fold seed and a comparison anchor.
    pub const ZERO: Money = Money(Decimal::ZERO);

    /// Construct from an exact [`Decimal`]. Total — any decimal is valid money.
    #[must_use]
    pub const fn from_decimal(value: Decimal) -> Self {
        Money(value)
    }

    /// Construct from a whole-currency-unit integer (e.g. `12` -> `$12.00`).
    #[must_use]
    pub fn from_major(units: i64) -> Self {
        Money(Decimal::from(units))
    }

    /// Construct from minor units (cents): `from_minor(1234)` -> `$12.34`.
    ///
    /// This is the safest constructor for tests and seed data because it cannot
    /// introduce a rounding error — the value is exact by construction.
    #[must_use]
    pub fn from_minor(cents: i64) -> Self {
        // 2-decimal-place scale, no precision loss for an i64 input.
        Money(Decimal::new(cents, 2))
    }

    /// `const` construction from minor units (cents), for pinned tolerance
    /// constants (`docs/AI_FEATURE_DESIGN.md §Phase 5`: `MONEY_BAND`).
    ///
    /// [`Money::from_minor`] calls [`Decimal::new`], which is NOT `const` in
    /// `rust_decimal` 1.x, so it cannot back a `const`. This constructor uses
    /// [`Decimal::from_parts`] (which IS `const`, exactly as
    /// `DeficitFinancingConfig`'s pinned ratio does) to build the same exact value
    /// at compile time. The full `i64` range is supported: the magnitude's low and
    /// high 32 bits feed `lo`/`mid` of the 96-bit mantissa (`hi = 0`), scale `2`.
    ///
    /// Exactness: `from_minor_const(cents) == from_minor(cents)` for every `i64`
    /// (pinned by a test).
    #[must_use]
    pub const fn from_minor_const(cents: i64) -> Self {
        let negative = cents < 0;
        let magnitude = cents.unsigned_abs();
        // Split the u64 magnitude across the low two 32-bit words of the 96-bit
        // mantissa; `hi` is always 0 because |i64| < 2^64.
        #[allow(clippy::cast_possible_truncation)]
        let lo = magnitude as u32;
        #[allow(clippy::cast_possible_truncation)]
        let mid = (magnitude >> 32) as u32;
        Money(Decimal::from_parts(lo, mid, 0, negative, 2))
    }

    /// Parse a decimal string (e.g. `"12.34"`, `"-5.98"`) into exact money.
    ///
    /// `DOMAIN-3`-style fallible constructor. Returns [`ValidationError::Money`]
    /// if the string is not a valid decimal.
    ///
    /// # Errors
    /// Returns [`ValidationError::Money`] when `raw` is not a parseable decimal.
    pub fn try_parse(field: &'static str, raw: &str) -> Result<Self, ValidationError> {
        raw.trim()
            .parse::<Decimal>()
            .map(Money)
            .map_err(|e| ValidationError::Money {
                field,
                reason: e.to_string(),
            })
    }

    /// The underlying exact decimal value.
    #[must_use]
    pub const fn as_decimal(&self) -> Decimal {
        self.0
    }

    /// `true` if this amount is exactly zero.
    #[must_use]
    pub fn is_zero(&self) -> bool {
        self.0.is_zero()
    }

    /// `true` if this amount is negative (an expense / outflow).
    #[must_use]
    pub fn is_negative(&self) -> bool {
        self.0.is_sign_negative() && !self.0.is_zero()
    }

    /// `true` if this amount is positive (income / inflow).
    #[must_use]
    pub fn is_positive(&self) -> bool {
        self.0.is_sign_positive() && !self.0.is_zero()
    }

    /// Absolute value.
    #[must_use]
    pub fn abs(&self) -> Self {
        Money(self.0.abs())
    }

    /// Round to whole cents (2 dp) using banker's rounding.
    ///
    /// This is the ONLY rounding point in the money type — call it explicitly
    /// at the boundaries where a non-cent value would otherwise leak (e.g.
    /// sinking-fund accrual = `amount / period_months`). Internal arithmetic
    /// stays exact until a deliberate rounding is requested.
    #[must_use]
    pub fn round_to_cents(&self) -> Self {
        Money(self.0.round_dp(2))
    }

    /// Divide into `n` parts, rounding each to cents.
    ///
    /// Used for sinking-fund monthly accrual and buffer-repayment installments
    /// (`amount / period_months`). Returns [`Money::ZERO`] for `n == 0` to avoid
    /// a panic (division by zero) — callers should validate `n > 0` upstream and
    /// treat zero as "no accrual".
    #[must_use]
    pub fn divide_into(&self, n: u32) -> Self {
        if n == 0 {
            return Money::ZERO;
        }
        Money(self.0 / Decimal::from(n)).round_to_cents()
    }
}

impl Add for Money {
    type Output = Money;
    fn add(self, rhs: Money) -> Money {
        Money(self.0 + rhs.0)
    }
}

impl Sub for Money {
    type Output = Money;
    fn sub(self, rhs: Money) -> Money {
        Money(self.0 - rhs.0)
    }
}

impl Neg for Money {
    type Output = Money;
    fn neg(self) -> Money {
        Money(-self.0)
    }
}

impl AddAssign for Money {
    fn add_assign(&mut self, rhs: Money) {
        self.0 += rhs.0;
    }
}

impl SubAssign for Money {
    fn sub_assign(&mut self, rhs: Money) {
        self.0 -= rhs.0;
    }
}

impl Sum for Money {
    fn sum<I: Iterator<Item = Money>>(iter: I) -> Money {
        iter.fold(Money::ZERO, Add::add)
    }
}

impl<'a> Sum<&'a Money> for Money {
    fn sum<I: Iterator<Item = &'a Money>>(iter: I) -> Money {
        iter.fold(Money::ZERO, |acc, m| acc + *m)
    }
}

impl std::fmt::Display for Money {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0.round_dp(2))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::Decimal;

    /// Parse helper for tests. The lint config denies `unwrap`/`expect`/`panic`
    /// even in test code, so we return [`Money::ZERO`] on the impossible-in-test
    /// error path rather than unwrapping; the assertions below would fail loudly
    /// if a value unexpectedly failed to parse.
    fn parse(raw: &str) -> Money {
        Money::try_parse("test", raw).unwrap_or(Money::ZERO)
    }

    #[test]
    fn from_minor_is_exact() {
        // Compare the whole Result so no value-unwrapping is needed.
        assert_eq!(Money::try_parse("a", "12.34"), Ok(Money::from_minor(1234)));
        assert_eq!(Money::try_parse("a", "-5.98"), Ok(Money::from_minor(-598)));
    }

    #[test]
    fn from_minor_const_matches_from_minor_across_the_range() {
        // The const constructor (for pinned tolerance constants) must equal the
        // runtime one for every i64, including the extremes and both signs.
        for cents in [
            0_i64,
            1,
            -1,
            99,
            -99,
            1_234,
            -598,
            5_000_000,
            -5_000_000,
            i64::MAX,
            i64::MIN + 1,
        ] {
            assert_eq!(
                Money::from_minor_const(cents),
                Money::from_minor(cents),
                "from_minor_const drifted from from_minor at {cents}"
            );
        }
        // The pinned one-cent band, available as a genuine const.
        const ONE_CENT: Money = Money::from_minor_const(1);
        assert_eq!(ONE_CENT, Money::from_minor(1));
    }

    #[test]
    fn float_trap_does_not_corrupt_money() {
        // The canonical IEEE-754 failure: 0.1 + 0.2 != 0.3. Money must not lose this.
        let sum = parse("0.1") + parse("0.2");
        assert_eq!(sum, parse("0.3"));
        assert_eq!(sum.as_decimal(), Decimal::new(3, 1));
    }

    #[test]
    fn sum_of_transactions_matches_decimal_oracle() {
        // Sum N awkward cents amounts; assert zero drift vs a Decimal oracle.
        let cents = [598_i64, 1739, 20263, -5000, 33, 1, 9999, -1234];
        let money_sum: Money = cents.iter().map(|c| Money::from_minor(*c)).sum();
        let oracle: Decimal = cents.iter().map(|c| Decimal::new(*c, 2)).sum();
        assert_eq!(money_sum.as_decimal(), oracle);
    }

    #[test]
    fn rolling_balance_forward_loses_zero_cents() {
        // Simulate the rolling-Other chain: each month's net adds to the balance.
        // Property: the running balance equals the exact sum of all nets.
        let monthly_nets = [
            Money::from_minor(21200),
            Money::from_minor(-15075),
            Money::from_minor(333),
            Money::from_minor(-1),
            Money::from_minor(99999),
        ];
        let mut balance = Money::ZERO;
        for net in monthly_nets {
            balance += net;
        }
        let oracle: Money = monthly_nets.iter().copied().sum();
        assert_eq!(balance, oracle);
        assert_eq!(balance, Money::from_minor(21200 - 15075 + 333 - 1 + 99999));
    }

    #[test]
    fn divide_into_rounds_to_cents() {
        // $100.00 / 3 = $33.33 (rounded), not a repeating decimal.
        let accrual = Money::from_major(100).divide_into(3);
        assert_eq!(accrual, Money::from_minor(3333));
    }

    #[test]
    fn divide_into_zero_is_safe_zero() {
        assert_eq!(Money::from_major(100).divide_into(0), Money::ZERO);
    }

    #[test]
    fn sign_predicates() {
        assert!(Money::from_minor(-1).is_negative());
        assert!(Money::from_minor(1).is_positive());
        assert!(Money::ZERO.is_zero());
        assert!(!Money::ZERO.is_negative());
        assert!(!Money::ZERO.is_positive());
    }

    #[test]
    fn try_parse_rejects_garbage() {
        assert!(matches!(
            Money::try_parse("amount", "not-a-number"),
            Err(ValidationError::Money {
                field: "amount",
                ..
            })
        ));
    }
}
