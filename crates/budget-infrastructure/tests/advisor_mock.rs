//! REQUIRED integration test — the `BUDGET-AI-1` hallucination firewall, proven
//! against the mock (`docs/AI_FEATURE_DESIGN.md §Phase 5`).
//!
//! Feeds the REQUIRED `gemini_hallucinated.json` fixture through
//! `MockInvestmentAdvisor(Hallucinated)` (the SAME wire→domain path the real
//! adapter will use), reconciles each recommendation against the matching
//! ground-truth snapshot, and asserts the firewall caught every fabrication:
//! at least one `UnknownTicker`, at least one `ValueMismatch`, and NOT ONE
//! recommendation reconciles to `Verified`. This is the whole reconciliation
//! firewall proven before a single real Gemini byte (Phase 6).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use chrono::Utc;
use rust_decimal::Decimal;

use budget_app_services::reconcile;
use budget_domain::enums::AccountType;
use budget_domain::ids::{PositionId, UserId};
use budget_domain::money::Money;
use budget_domain::portfolio::{
    ClaimSubject, InvestmentAdvisor, NetWorth, PortfolioSnapshot, Position, PriceProvenance,
    PriceQuote, PricedPosition, Ticker, UnverifiedReason, ValidationOutcome,
};

use budget_infrastructure::{MockInvestmentAdvisor, MockMode};

/// The ground-truth snapshot the hallucinated fixture is fabricating AGAINST:
/// AAPL truly $1800 (the fixture cites $50,000) and the buffer truly $5000 (the
/// fixture cites $3000). TSLA is NOT present (the fixture cites a $12,000 TSLA
/// position that does not exist).
fn ground_truth_snapshot() -> PortfolioSnapshot {
    let now = Utc::now();
    let aapl = PricedPosition {
        position: Position {
            id: PositionId::generate(),
            user_id: UserId::generate(),
            ticker: Ticker::try_new("AAPL").unwrap(),
            account_label: "Brokerage".to_owned(),
            account_type: AccountType::Investment,
            shares: Decimal::new(10, 0),
            cost_basis: Some(Money::from_minor(150_000)),
            created_at: now,
            updated_at: now,
        },
        quote: Some(PriceQuote {
            price: Money::from_minor(18_000),
            provenance: PriceProvenance::Market {
                source: "test".to_owned(),
            },
            as_of: now,
        }),
        market_value: Some(Money::from_minor(180_000)), // $1800
    };
    PortfolioSnapshot {
        user_id: UserId::generate(),
        positions: vec![aapl],
        cash_balances: vec![],
        buffer_total: Money::from_minor(500_000), // $5000
        net_worth: NetWorth {
            total_cash: Money::from_minor(500_000),
            total_positions: Money::from_minor(180_000),
            liabilities: Money::ZERO,
            total: Money::from_minor(680_000),
        },
        total_invested: Money::from_minor(180_000),
        captured_at: now,
    }
}

#[tokio::test]
async fn hallucinated_fixture_is_caught_by_the_reconcile_firewall() {
    let advisor = MockInvestmentAdvisor::new(MockMode::Hallucinated);
    let snapshot = ground_truth_snapshot();

    let output = advisor
        .recommend(&snapshot)
        .await
        .expect("mock fixture parses through the wire->domain path");

    // The fixture carries exactly three fabricated recommendations.
    assert_eq!(output.recommendations.len(), 3);

    // Reconcile each recommendation; collect the per-claim outcomes.
    let results: Vec<_> = output
        .recommendations
        .iter()
        .map(|rec| reconcile(rec, &snapshot))
        .collect();

    let all_claim_outcomes: Vec<&ValidationOutcome> = results
        .iter()
        .flat_map(|r| r.per_claim.iter().map(|(_, o)| o))
        .collect();

    // (a) at least one UnknownTicker (the fabricated TSLA position).
    let unknown_ticker_count = all_claim_outcomes
        .iter()
        .filter(|o| {
            matches!(
                o,
                ValidationOutcome::Unverified(UnverifiedReason::UnknownTicker(_))
            )
        })
        .count();
    assert!(
        unknown_ticker_count >= 1,
        "expected >=1 UnknownTicker (the fabricated TSLA), got {unknown_ticker_count}"
    );

    // (b) at least one ValueMismatch (AAPL $50k vs $1800, buffer $3000 vs $5000).
    let value_mismatch_count = all_claim_outcomes
        .iter()
        .filter(|o| {
            matches!(
                o,
                ValidationOutcome::Unverified(UnverifiedReason::ValueMismatch { .. })
            )
        })
        .count();
    assert!(
        value_mismatch_count >= 1,
        "expected >=1 ValueMismatch, got {value_mismatch_count}"
    );

    // (c) NOT ONE recommendation reconciles to Verified — the firewall caught
    // every fabrication.
    let any_verified = results
        .iter()
        .any(|r| r.outcome == ValidationOutcome::Verified);
    assert!(
        !any_verified,
        "BUDGET-AI-1 firewall breach: a hallucinated recommendation was marked Verified"
    );

    // Belt-and-suspenders: the specific TSLA / AAPL / buffer subjects are present
    // among the claims (the fixture wired the three fabrications we expect).
    let subjects: Vec<&ClaimSubject> = output
        .recommendations
        .iter()
        .flat_map(|r| r.claims.iter().map(|c| &c.subject))
        .collect();
    assert!(subjects.iter().any(|s| matches!(
        s,
        ClaimSubject::Position { ticker } if ticker.as_str() == "TSLA"
    )));
    assert!(subjects.contains(&&ClaimSubject::Buffer));
}
