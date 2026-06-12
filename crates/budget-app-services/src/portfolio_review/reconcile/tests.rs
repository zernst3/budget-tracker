//! Exhaustive reconcile tests — the `BUDGET-AI-1` firewall, mock-only
//! (`docs/AI_FEATURE_DESIGN.md §Phase 5`, `ORCH-NEW-PATH-TESTS-1`).
//!
//! Every arm of the exhaustive `reconcile_claim` match is exercised, including
//! the `CostBasisGain` verify path and its no-cost-basis `Unverified` path. The
//! canonical `base_snapshot()` is shared by the use-case tests so the ground
//! truth is one fixture, not several drifting ones.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use chrono::Utc;
use rust_decimal::Decimal;

use budget_domain::enums::AccountType;
use budget_domain::ids::{PositionId, UserId};
use budget_domain::money::Money;
use budget_domain::portfolio::{
    Claim, ClaimSubject, Confidence, NetWorth, PortfolioSnapshot, Position, PriceProvenance,
    PriceQuote, PricedPosition, Recommendation, ShareProvenance, Ticker, UnverifiedReason,
    ValidationOutcome,
};

use super::{MONEY_BAND, reconcile};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn ticker(s: &str) -> Ticker {
    Ticker::try_new(s).unwrap()
}

fn priced(
    sym: &str,
    shares: i64,
    price_cents: i64,
    cost_basis: Option<Money>,
    resolved: bool,
) -> PricedPosition {
    let now = Utc::now();
    let position = Position {
        id: PositionId::generate(),
        user_id: UserId::generate(),
        ticker: ticker(sym),
        account_label: "Brokerage".to_owned(),
        account_type: AccountType::Investment,
        shares: Decimal::new(shares, 0),
        cost_basis,
        drip_enabled: false,
        baseline_as_of: now,
        created_at: now,
        updated_at: now,
    };
    let (quote, market_value) = if resolved {
        (
            Some(PriceQuote {
                price: Money::from_minor(price_cents),
                provenance: PriceProvenance::Market {
                    source: "test".to_owned(),
                },
                as_of: now,
            }),
            Some(Money::from_minor(shares * price_cents)),
        )
    } else {
        (None, None)
    };
    PricedPosition {
        position,
        quote,
        market_value,
        share_provenance: ShareProvenance::Uploaded,
    }
}

/// The canonical ground-truth snapshot (`§Phase 5` tests):
/// AAPL $180×10=$1800, NVDA $500×5=$2500, total_invested $4300, buffer $5000,
/// total_cash $6000, net_worth.total $10300.
fn base_snapshot() -> PortfolioSnapshot {
    PortfolioSnapshot {
        user_id: UserId::generate(),
        positions: vec![
            priced("AAPL", 10, 18_000, Some(Money::from_minor(150_000)), true),
            priced("NVDA", 5, 50_000, None, true),
        ],
        cash_balances: vec![],
        buffer_total: Money::from_minor(500_000),
        net_worth: NetWorth {
            total_cash: Money::from_minor(600_000),
            total_positions: Money::from_minor(430_000),
            liabilities: Money::ZERO,
            total: Money::from_minor(1_030_000),
        },
        total_invested: Money::from_minor(430_000),
        captured_at: Utc::now(),
    }
}

/// A `base_snapshot` whose AAPL quote went stale (`market_value: None`).
fn snapshot_with_stale_aapl() -> PortfolioSnapshot {
    let mut snap = base_snapshot();
    snap.positions[0] = priced("AAPL", 10, 18_000, Some(Money::from_minor(150_000)), false);
    snap
}

fn rec(claims: Vec<Claim>) -> Recommendation {
    Recommendation {
        title: "t".to_owned(),
        rationale: "r".to_owned(),
        confidence: Confidence::High,
        claims,
    }
}

fn position_claim(sym: &str, cents: i64, pct: Option<Decimal>) -> Claim {
    Claim {
        subject: ClaimSubject::Position {
            ticker: ticker(sym),
        },
        cited_value: Money::from_minor(cents),
        cited_percentage: pct,
    }
}

fn buffer_claim(cents: i64, pct: Option<Decimal>) -> Claim {
    Claim {
        subject: ClaimSubject::Buffer,
        cited_value: Money::from_minor(cents),
        cited_percentage: pct,
    }
}

fn net_worth_claim(cents: i64, pct: Option<Decimal>) -> Claim {
    Claim {
        subject: ClaimSubject::NetWorth,
        cited_value: Money::from_minor(cents),
        cited_percentage: pct,
    }
}

fn cost_basis_gain_claim(sym: &str, cents: i64, pct: Option<Decimal>) -> Claim {
    Claim {
        subject: ClaimSubject::CostBasisGain {
            ticker: ticker(sym),
        },
        cited_value: Money::from_minor(cents),
        cited_percentage: pct,
    }
}

fn single(claim: Claim) -> ValidationOutcome {
    reconcile(&rec(vec![claim]), &base_snapshot()).outcome
}

// ---------------------------------------------------------------------------
// Position arm
// ---------------------------------------------------------------------------

#[test]
fn position_exact_value_verifies() {
    assert_eq!(
        single(position_claim("AAPL", 180_000, None)),
        ValidationOutcome::Verified
    );
}

#[test]
fn position_within_one_cent_verifies() {
    // $1800.01 vs $1800.00 == within MONEY_BAND.
    assert_eq!(
        single(position_claim("AAPL", 180_001, None)),
        ValidationOutcome::Verified
    );
}

#[test]
fn position_two_cents_off_is_value_mismatch() {
    let out = single(position_claim("AAPL", 180_002, None));
    assert!(matches!(
        out,
        ValidationOutcome::Unverified(UnverifiedReason::ValueMismatch { .. })
    ));
}

#[test]
fn position_fifty_thousand_hallucination_is_value_mismatch() {
    let out = single(position_claim("AAPL", 5_000_000, None));
    assert_eq!(
        out,
        ValidationOutcome::Unverified(UnverifiedReason::ValueMismatch {
            cited: Money::from_minor(5_000_000),
            ground_truth: Money::from_minor(180_000),
        })
    );
}

#[test]
fn position_unknown_ticker_is_unknown_ticker() {
    assert_eq!(
        single(position_claim("TSLA", 100_000, None)),
        ValidationOutcome::Unverified(UnverifiedReason::UnknownTicker("TSLA".to_owned()))
    );
}

#[test]
fn position_missing_market_data_is_missing_market_data() {
    let out = reconcile(
        &rec(vec![position_claim("AAPL", 180_000, None)]),
        &snapshot_with_stale_aapl(),
    )
    .outcome;
    assert_eq!(
        out,
        ValidationOutcome::Unverified(UnverifiedReason::MissingMarketData("AAPL".to_owned()))
    );
}

#[test]
fn position_correct_ratio_verifies() {
    // AAPL $1800 / total_invested $4300 = 0.4186 -> round_dp(1) = 0.4.
    assert_eq!(
        single(position_claim("AAPL", 180_000, Some(Decimal::new(4, 1)))),
        ValidationOutcome::Verified
    );
}

#[test]
fn position_wrong_ratio_is_percentage_mismatch() {
    // Cite 0.9 vs ground 0.4.
    let out = single(position_claim("AAPL", 180_000, Some(Decimal::new(9, 1))));
    assert!(matches!(
        out,
        ValidationOutcome::Unverified(UnverifiedReason::PercentageMismatch { .. })
    ));
}

// ---------------------------------------------------------------------------
// Buffer arm
// ---------------------------------------------------------------------------

#[test]
fn buffer_exact_verifies() {
    assert_eq!(
        single(buffer_claim(500_000, None)),
        ValidationOutcome::Verified
    );
}

#[test]
fn buffer_within_band_verifies() {
    assert_eq!(
        single(buffer_claim(499_999, None)),
        ValidationOutcome::Verified
    );
}

#[test]
fn buffer_wrong_figure_is_value_mismatch() {
    let out = single(buffer_claim(300_000, None));
    assert_eq!(
        out,
        ValidationOutcome::Unverified(UnverifiedReason::ValueMismatch {
            cited: Money::from_minor(300_000),
            ground_truth: Money::from_minor(500_000),
        })
    );
}

#[test]
fn buffer_with_percentage_is_malformed_claim() {
    let out = single(buffer_claim(500_000, Some(Decimal::new(1, 1))));
    assert!(matches!(
        out,
        ValidationOutcome::Unverified(UnverifiedReason::MalformedClaim(_))
    ));
}

// ---------------------------------------------------------------------------
// NetWorth arm
// ---------------------------------------------------------------------------

#[test]
fn net_worth_exact_verifies() {
    assert_eq!(
        single(net_worth_claim(1_030_000, None)),
        ValidationOutcome::Verified
    );
}

#[test]
fn net_worth_wrong_is_value_mismatch() {
    let out = single(net_worth_claim(999_999, None));
    assert!(matches!(
        out,
        ValidationOutcome::Unverified(UnverifiedReason::ValueMismatch { .. })
    ));
}

#[test]
fn net_worth_with_percentage_is_malformed_claim() {
    let out = single(net_worth_claim(1_030_000, Some(Decimal::new(1, 1))));
    assert!(matches!(
        out,
        ValidationOutcome::Unverified(UnverifiedReason::MalformedClaim(_))
    ));
}

// ---------------------------------------------------------------------------
// CostBasisGain arm (added 2026-06-11)
// ---------------------------------------------------------------------------

#[test]
fn cost_basis_gain_exact_verifies() {
    // AAPL market_value $1800 - cost_basis $1500 = $300 gain.
    assert_eq!(
        single(cost_basis_gain_claim("AAPL", 30_000, None)),
        ValidationOutcome::Verified
    );
}

#[test]
fn cost_basis_gain_wrong_is_value_mismatch() {
    let out = single(cost_basis_gain_claim("AAPL", 99_999, None));
    assert_eq!(
        out,
        ValidationOutcome::Unverified(UnverifiedReason::ValueMismatch {
            cited: Money::from_minor(99_999),
            ground_truth: Money::from_minor(30_000),
        })
    );
}

#[test]
fn cost_basis_gain_no_cost_basis_is_missing_market_data() {
    // NVDA has cost_basis None -> the gain is uncomputable -> MissingMarketData
    // (the documented decision reuses this reason, carrying the ticker).
    assert_eq!(
        single(cost_basis_gain_claim("NVDA", 10_000, None)),
        ValidationOutcome::Unverified(UnverifiedReason::MissingMarketData("NVDA".to_owned()))
    );
}

#[test]
fn cost_basis_gain_unknown_ticker_is_unknown_ticker() {
    assert_eq!(
        single(cost_basis_gain_claim("TSLA", 10_000, None)),
        ValidationOutcome::Unverified(UnverifiedReason::UnknownTicker("TSLA".to_owned()))
    );
}

#[test]
fn cost_basis_gain_missing_market_data_when_quote_stale() {
    let out = reconcile(
        &rec(vec![cost_basis_gain_claim("AAPL", 30_000, None)]),
        &snapshot_with_stale_aapl(),
    )
    .outcome;
    assert_eq!(
        out,
        ValidationOutcome::Unverified(UnverifiedReason::MissingMarketData("AAPL".to_owned()))
    );
}

#[test]
fn cost_basis_gain_with_percentage_is_malformed_claim() {
    // Percentage is Position-only; on CostBasisGain it is malformed (first guard).
    let out = single(cost_basis_gain_claim(
        "AAPL",
        30_000,
        Some(Decimal::new(1, 1)),
    ));
    assert!(matches!(
        out,
        ValidationOutcome::Unverified(UnverifiedReason::MalformedClaim(_))
    ));
}

// ---------------------------------------------------------------------------
// Multi-claim aggregation + vacuous
// ---------------------------------------------------------------------------

#[test]
fn any_unverified_claim_makes_the_recommendation_unverified() {
    let result = reconcile(
        &rec(vec![
            position_claim("AAPL", 180_000, None), // verified
            buffer_claim(300_000, None),           // mismatch
        ]),
        &base_snapshot(),
    );
    assert!(matches!(
        result.outcome,
        ValidationOutcome::Unverified(UnverifiedReason::ValueMismatch { .. })
    ));
    assert_eq!(result.per_claim.len(), 2);
    assert_eq!(result.per_claim[0].1, ValidationOutcome::Verified);
    assert!(matches!(
        result.per_claim[1].1,
        ValidationOutcome::Unverified(_)
    ));
}

#[test]
fn all_three_arms_verified_makes_the_recommendation_verified() {
    let result = reconcile(
        &rec(vec![
            position_claim("AAPL", 180_000, None),
            buffer_claim(500_000, None),
            net_worth_claim(1_030_000, None),
        ]),
        &base_snapshot(),
    );
    assert_eq!(result.outcome, ValidationOutcome::Verified);
    assert_eq!(result.per_claim.len(), 3);
    assert!(
        result
            .per_claim
            .iter()
            .all(|(_, o)| *o == ValidationOutcome::Verified)
    );
}

#[test]
fn zero_claim_recommendation_is_vacuously_verified() {
    let result = reconcile(&rec(vec![]), &base_snapshot());
    assert_eq!(result.outcome, ValidationOutcome::Verified);
    assert!(result.per_claim.is_empty());
}

#[test]
fn money_band_is_exactly_one_cent() {
    // Pin the firewall tolerance: the band is $0.01 exactly.
    assert_eq!(MONEY_BAND, Money::from_minor(1));
}
