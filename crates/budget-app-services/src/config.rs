//! App-level configuration values (`SERVICE-DI-1`).
//!
//! These are fixed, single-tenant operating parameters of the budget tracker —
//! the same category as the fixed home timezone (`America/New_York`, `D2`) wired
//! into [`crate::month_lifecycle`]: an app-wide constant with a default, threaded
//! into the service that consumes it rather than read from a global. Keeping the
//! value behind a small config struct (instead of a bare `const` inside the
//! service) means a future per-budget override is a constructor change, not a
//! re-architecture, and tests can pin an exact threshold.

use rust_decimal::Decimal;

/// The deficit-financing threshold (`SPEC §12` D9, `BUDGET-DEFICIT-FINANCING-1`).
///
/// A closed month's deficit is only OFFERED for financing when it exceeds
/// [`DeficitFinancingConfig::threshold_ratio`] × the next month's Other budget.
/// The default is **75%** (`0.75`); below the threshold (or if the offer is
/// declined) the deficit rolls forward in full per `SPEC §4.3`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeficitFinancingConfig {
    /// The fraction of next month's Other budget the deficit must exceed for the
    /// financing offer to appear. Default `0.75`.
    pub threshold_ratio: Decimal,
}

/// The default deficit-financing threshold ratio: 75% (`SPEC §12` D9). Expressed
/// as `75 / 100` so the value is exact in [`Decimal`] (no float).
const DEFAULT_DEFICIT_THRESHOLD_RATIO: Decimal = Decimal::from_parts(75, 0, 0, false, 2);

impl Default for DeficitFinancingConfig {
    fn default() -> Self {
        Self {
            threshold_ratio: DEFAULT_DEFICIT_THRESHOLD_RATIO,
        }
    }
}

impl DeficitFinancingConfig {
    /// Construct with an explicit threshold ratio (e.g. tests pinning an exact
    /// boundary). Most callers use [`DeficitFinancingConfig::default`] (75%).
    #[must_use]
    pub const fn with_threshold(threshold_ratio: Decimal) -> Self {
        Self { threshold_ratio }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::{DEFAULT_DEFICIT_THRESHOLD_RATIO, DeficitFinancingConfig};
    use rust_decimal::Decimal;

    #[test]
    fn default_is_exactly_three_quarters() {
        // 0.75 exactly — no float drift (BUDGET-MONEY-1 spirit for ratios).
        assert_eq!(DEFAULT_DEFICIT_THRESHOLD_RATIO, Decimal::new(75, 2));
        assert_eq!(
            DeficitFinancingConfig::default().threshold_ratio,
            Decimal::new(75, 2)
        );
    }

    #[test]
    fn with_threshold_overrides() {
        let cfg = DeficitFinancingConfig::with_threshold(Decimal::new(90, 2));
        assert_eq!(cfg.threshold_ratio, Decimal::new(90, 2));
    }
}
