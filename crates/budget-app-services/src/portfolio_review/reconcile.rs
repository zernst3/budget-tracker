//! Reconciliation — the `BUDGET-AI-1` firewall (`docs/AI_FEATURE_DESIGN.md
//! §Phase 5`, §0.2).
//!
//! [`reconcile`] checks each model [`Recommendation`]'s [`Claim`]s against the
//! ground-truth [`PortfolioSnapshot`] and produces a [`ValidationOutcome`] per
//! claim plus the recommendation's worst-case outcome. The core is
//! [`reconcile_claim`]: an **exhaustive `match` over every [`ClaimSubject`]
//! variant with NO `_` wildcard arm** (`BUDGET-AI-1`). Adding a fifth subject is
//! a compile error here until a real reconcile arm is written — that is the
//! mechanical enforcement that the model can never cite a figure the system does
//! not check.
//!
//! ## Tolerance constants (pinned)
//!
//! - [`MONEY_BAND`] — the absolute money band, pinned at one cent, built via the
//!   `const` [`Money::from_minor_const`] so the value is exact by construction
//!   (mirrors `DeficitFinancingConfig`'s pinned-ratio style).
//! - [`PERCENT_PRECISION_DP`] — the `% of portfolio` ratio is compared at one
//!   decimal place.

use rust_decimal::Decimal;

use budget_domain::money::Money;
use budget_domain::portfolio::{
    Claim, ClaimSubject, PortfolioSnapshot, Recommendation, UnverifiedReason, ValidationOutcome,
};

/// Absolute money band: a cited figure within `|cited - ground_truth| <=
/// MONEY_BAND` (after both are exact cents) reconciles. PINNED at one cent.
///
/// Built with the `const` [`Money::from_minor_const`] (`Decimal::new` is not
/// `const` in `rust_decimal` 1.x), so the band is exact at compile time.
pub const MONEY_BAND: Money = Money::from_minor_const(1);

/// The `% of portfolio` ratio is rounded to this many decimal places before the
/// equality check. PINNED at 1.
pub const PERCENT_PRECISION_DP: u32 = 1;

/// The reconciliation result for one [`Recommendation`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconcileResult {
    /// The recommendation's displayed outcome — the WORST across its claims (any
    /// `Unverified` makes the whole recommendation `Unverified`); a zero-claim
    /// recommendation is `Verified` (vacuous).
    pub outcome: ValidationOutcome,
    /// The per-claim outcomes, paired with the claim's subject (for the UI).
    pub per_claim: Vec<(ClaimSubject, ValidationOutcome)>,
}

/// Reconcile one [`Recommendation`] against the ground-truth `snap`.
///
/// Each claim is checked by [`reconcile_claim`]; the recommendation outcome is
/// the first `Unverified` across its claims, else `Verified`.
#[must_use]
pub fn reconcile(rec: &Recommendation, snap: &PortfolioSnapshot) -> ReconcileResult {
    let per_claim: Vec<(ClaimSubject, ValidationOutcome)> = rec
        .claims
        .iter()
        .map(|claim| (claim.subject.clone(), reconcile_claim(claim, snap)))
        .collect();

    // Worst-case: the first Unverified, else Verified (vacuously true for zero
    // claims).
    let outcome = per_claim
        .iter()
        .find_map(|(_, o)| match o {
            ValidationOutcome::Unverified(_) => Some(o.clone()),
            ValidationOutcome::Verified => None,
        })
        .unwrap_or(ValidationOutcome::Verified);

    ReconcileResult { outcome, per_claim }
}

/// Reconcile a single [`Claim`] against the ground-truth `snap`.
///
/// **Exhaustive over every [`ClaimSubject`] variant, NO `_` arm (`BUDGET-AI-1`).**
fn reconcile_claim(claim: &Claim, snap: &PortfolioSnapshot) -> ValidationOutcome {
    // Guard (first): a `cited_percentage` is meaningful ONLY for a `Position`
    // subject. On any non-`Position` subject it is a structurally malformed claim
    // (covers Buffer / NetWorth / CostBasisGain in one place, §Phase 5).
    if claim.cited_percentage.is_some() && !matches!(claim.subject, ClaimSubject::Position { .. }) {
        return ValidationOutcome::Unverified(UnverifiedReason::MalformedClaim(
            "cited_percentage is only valid for a Position claim".to_owned(),
        ));
    }

    match &claim.subject {
        ClaimSubject::Position { ticker } => reconcile_position(claim, ticker.as_str(), snap),
        ClaimSubject::Buffer => reconcile_money(claim.cited_value, snap.buffer_total),
        ClaimSubject::NetWorth => reconcile_money(claim.cited_value, snap.net_worth.total),
        ClaimSubject::CostBasisGain { ticker } => {
            reconcile_cost_basis_gain(claim, ticker.as_str(), snap)
        }
    }
}

/// Reconcile a `Position` claim: market-value match + optional `% of portfolio`.
fn reconcile_position(claim: &Claim, ticker: &str, snap: &PortfolioSnapshot) -> ValidationOutcome {
    let Some(priced) = snap
        .positions
        .iter()
        .find(|p| p.position.ticker.as_str() == ticker)
    else {
        return ValidationOutcome::Unverified(UnverifiedReason::UnknownTicker(ticker.to_owned()));
    };
    let Some(market_value) = priced.market_value else {
        return ValidationOutcome::Unverified(UnverifiedReason::MissingMarketData(
            ticker.to_owned(),
        ));
    };

    if !within_band(claim.cited_value, market_value) {
        return ValidationOutcome::Unverified(UnverifiedReason::ValueMismatch {
            cited: claim.cited_value,
            ground_truth: market_value,
        });
    }

    // Optional `% of portfolio` (a RATIO in [0,1]) vs market_value/total_invested.
    if let Some(cited_pct) = claim.cited_percentage {
        let total = snap.total_invested.as_decimal();
        let ground = if total.is_zero() {
            Decimal::ZERO
        } else {
            (market_value.as_decimal() / total).round_dp(PERCENT_PRECISION_DP)
        };
        if cited_pct.round_dp(PERCENT_PRECISION_DP) != ground {
            return ValidationOutcome::Unverified(UnverifiedReason::PercentageMismatch {
                cited: cited_pct,
                ground_truth: ground,
            });
        }
    }

    ValidationOutcome::Verified
}

/// Reconcile a `CostBasisGain` claim: ground truth = `market_value -
/// cost_basis` (added 2026-06-11, §Phase 5).
///
/// `ticker` not found → `UnknownTicker`. Found but no resolved `market_value`
/// (stale quote) → `MissingMarketData`. Found but the position's `cost_basis` is
/// `None` → `MissingMarketData` as well (the unrealized gain cannot be computed
/// without cost basis; the documented decision reuses the "a required figure is
/// missing" reason rather than adding a dedicated variant).
fn reconcile_cost_basis_gain(
    claim: &Claim,
    ticker: &str,
    snap: &PortfolioSnapshot,
) -> ValidationOutcome {
    let Some(priced) = snap
        .positions
        .iter()
        .find(|p| p.position.ticker.as_str() == ticker)
    else {
        return ValidationOutcome::Unverified(UnverifiedReason::UnknownTicker(ticker.to_owned()));
    };
    let Some(market_value) = priced.market_value else {
        return ValidationOutcome::Unverified(UnverifiedReason::MissingMarketData(
            ticker.to_owned(),
        ));
    };
    let Some(cost_basis) = priced.position.cost_basis else {
        // No cost basis -> the gain is uncomputable; reuse MissingMarketData
        // (documented decision 2026-06-11), which carries the ticker.
        return ValidationOutcome::Unverified(UnverifiedReason::MissingMarketData(
            ticker.to_owned(),
        ));
    };

    let ground_truth = market_value - cost_basis;
    if within_band(claim.cited_value, ground_truth) {
        ValidationOutcome::Verified
    } else {
        ValidationOutcome::Unverified(UnverifiedReason::ValueMismatch {
            cited: claim.cited_value,
            ground_truth,
        })
    }
}

/// Reconcile a plain money figure (`Buffer` / `NetWorth`) against ground truth.
fn reconcile_money(cited: Money, ground_truth: Money) -> ValidationOutcome {
    if within_band(cited, ground_truth) {
        ValidationOutcome::Verified
    } else {
        ValidationOutcome::Unverified(UnverifiedReason::ValueMismatch {
            cited,
            ground_truth,
        })
    }
}

/// `true` when `|cited - ground_truth| <= MONEY_BAND` after rounding both to
/// cents (the comparison is exact `Decimal` arithmetic; no `f64`).
fn within_band(cited: Money, ground_truth: Money) -> bool {
    let diff = (cited.round_to_cents() - ground_truth.round_to_cents()).abs();
    diff <= MONEY_BAND
}

#[cfg(test)]
mod tests;
