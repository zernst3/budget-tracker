//! Pinned DRIP estimation parameters (`docs/DRIP_REALTIME_DESIGN.md §2.2/§2.3`,
//! Open Item 2).
//!
//! The same "fixed operating parameter behind a small config struct" pattern as
//! [`crate::config::DeficitFinancingConfig`]: a tunable default threaded into the
//! service, so a future per-position override is a constructor change, not a
//! re-architecture, and tests can pin exact values.

use rust_decimal::Decimal;

/// The conservative DRIP estimation parameters.
///
/// `buffer` is the haircut applied to the ACCRETED shares only (the baseline is
/// never haircut, §2.2); `share_dp` is the decimal places accreted shares are
/// FLOOR-rounded to (floor, not round — deliberately conservative, §2.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DripConfig {
    /// `DRIP_BUFFER` — the fraction shaved off accreted shares (default `0.10`).
    /// `buffer_factor = 1 - buffer`. Scoped to the estimate, NOT the whole
    /// position (§2.2).
    pub buffer: Decimal,
    /// `DRIP_SHARE_DP` — decimal places accreted shares are floored to (default
    /// `3`, matching common broker fractional-DRIP precision, §2.3).
    pub share_dp: u32,
}

/// The default DRIP buffer: 10% (`0.10`). Expressed as `10 / 100` so the value is
/// exact in [`Decimal`] (no float, `BUDGET-MONEY-1` spirit for ratios).
const DEFAULT_DRIP_BUFFER: Decimal = Decimal::from_parts(10, 0, 0, false, 2);

/// The default fractional-share floor precision: 3 dp (`DRIP_SHARE_DP`).
const DEFAULT_DRIP_SHARE_DP: u32 = 3;

impl Default for DripConfig {
    fn default() -> Self {
        Self {
            buffer: DEFAULT_DRIP_BUFFER,
            share_dp: DEFAULT_DRIP_SHARE_DP,
        }
    }
}

impl DripConfig {
    /// Construct with explicit values (tests pinning exact buffer / precision).
    /// Most callers use [`DripConfig::default`] (0.10 / 3 dp).
    #[must_use]
    pub const fn new(buffer: Decimal, share_dp: u32) -> Self {
        Self { buffer, share_dp }
    }
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_DRIP_BUFFER, DEFAULT_DRIP_SHARE_DP, DripConfig};
    use rust_decimal::Decimal;

    #[test]
    fn default_buffer_is_exactly_ten_percent() {
        assert_eq!(DEFAULT_DRIP_BUFFER, Decimal::new(10, 2));
        assert_eq!(DripConfig::default().buffer, Decimal::new(10, 2));
    }

    #[test]
    fn default_share_dp_is_three() {
        assert_eq!(DEFAULT_DRIP_SHARE_DP, 3);
        assert_eq!(DripConfig::default().share_dp, 3);
    }

    #[test]
    fn new_overrides_both() {
        let cfg = DripConfig::new(Decimal::new(5, 2), 4);
        assert_eq!(cfg.buffer, Decimal::new(5, 2));
        assert_eq!(cfg.share_dp, 4);
    }
}
