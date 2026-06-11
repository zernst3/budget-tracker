//! Terminal-state tests for `GeneratePortfolioReview` — the mock-only firewall
//! (`docs/AI_FEATURE_DESIGN.md §Phase 5`, `ORCH-NEW-PATH-TESTS-1`).
//!
//! Each terminal state is proven with fake ports (no DB, no network):
//!   - `EmptyPortfolio` — proven by a panic-if-called advisor (no model call).
//!   - `MalformedOutput` — `Err(Parse)` persists a run with the raw output.
//!   - `NoVerifiableInsights` — zero recs, and a separate all-claims-unverified.
//!   - `Completed` — one verified recommendation.
//!   - stale quote — `quote(AAPL) -> Ok(None)` with a citing AAPL claim degrades
//!     to `outcomes[0] == Unverified(MissingMarketData("AAPL"))` (the indexed
//!     `outcomes` read, §0.4).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::any::Any;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::Utc;
use rust_decimal::Decimal;

use budget_domain::RepositoryError;
use budget_domain::enums::AccountType;
use budget_domain::ids::{PositionId, UserId};
use budget_domain::money::Money;
use budget_domain::portfolio::{
    AdvisorError, AdvisorOutput, CashBalance, CashBalanceSource, Claim, ClaimSubject, Confidence,
    InvestmentAdvisor, MarketDataError, MarketDataProvider, Position, PositionSource,
    PriceProvenance, PriceQuote, Recommendation, ReviewRun, ReviewTerminalState, Ticker,
    UnverifiedReason, ValidationOutcome,
};
use budget_domain::repositories::ReviewRunRepository;
use budget_domain::uow::{UnitOfWork, UowFuture, UowProvider};

use super::GeneratePortfolioReview;

// ---------------------------------------------------------------------------
// UoW fakes
// ---------------------------------------------------------------------------

struct FakeUow;
impl UnitOfWork for FakeUow {
    fn as_any(&self) -> &dyn Any {
        self
    }
}

type BoxedUowClosure<'a> =
    Box<dyn for<'u> FnOnce(&'u dyn UnitOfWork) -> UowFuture<'u, Box<dyn Any + Send>> + Send + 'a>;

struct FakeUowProvider;

#[async_trait]
impl UowProvider for FakeUowProvider {
    async fn run_boxed(
        &self,
        f: BoxedUowClosure<'_>,
    ) -> Result<Box<dyn Any + Send>, RepositoryError> {
        let uow = FakeUow;
        let handle: &dyn UnitOfWork = &uow;
        f(handle).await
    }
}

// ---------------------------------------------------------------------------
// Port fakes
// ---------------------------------------------------------------------------

struct FakePositions(Vec<Position>);
#[async_trait]
impl PositionSource for FakePositions {
    async fn positions_for_user(&self, _u: UserId) -> Result<Vec<Position>, RepositoryError> {
        Ok(self.0.clone())
    }
}

struct FakeBalances(Vec<CashBalance>);
#[async_trait]
impl CashBalanceSource for FakeBalances {
    async fn balances_for_user(&self, _u: UserId) -> Result<Vec<CashBalance>, RepositoryError> {
        Ok(self.0.clone())
    }
}

/// A market provider that resolves a fixed quote for any ticker, except those in
/// `degrade` (which return `Ok(None)` — a stale/missing quote).
struct FakeMarket {
    price_cents: i64,
    degrade: Vec<String>,
}
#[async_trait]
impl MarketDataProvider for FakeMarket {
    async fn quote(&self, ticker: &Ticker) -> Result<Option<PriceQuote>, MarketDataError> {
        if self.degrade.iter().any(|t| t == ticker.as_str()) {
            return Ok(None);
        }
        Ok(Some(PriceQuote {
            price: Money::from_minor(self.price_cents),
            provenance: PriceProvenance::Market {
                source: "test".to_owned(),
            },
            as_of: Utc::now(),
        }))
    }
}

/// An advisor that panics if `recommend` is ever called — proves the
/// empty-portfolio short-circuit makes NO model call.
struct PanicAdvisor;
#[async_trait]
impl InvestmentAdvisor for PanicAdvisor {
    async fn recommend(
        &self,
        _s: &budget_domain::portfolio::PortfolioSnapshot,
    ) -> Result<AdvisorOutput, AdvisorError> {
        panic!("advisor must NOT be called for an empty portfolio");
    }
    fn model_id(&self) -> &str {
        "panic-advisor"
    }
}

/// An advisor returning a fixed result (or error) from `recommend`.
struct ScriptedAdvisor {
    result: Result<AdvisorOutput, AdvisorError>,
}
#[async_trait]
impl InvestmentAdvisor for ScriptedAdvisor {
    async fn recommend(
        &self,
        _s: &budget_domain::portfolio::PortfolioSnapshot,
    ) -> Result<AdvisorOutput, AdvisorError> {
        self.result.clone()
    }
    fn model_id(&self) -> &str {
        "scripted-advisor"
    }
}

/// A review-run repository that records every persisted run.
#[derive(Default)]
struct RecordingRepo {
    runs: Mutex<Vec<ReviewRun>>,
}
#[async_trait]
impl ReviewRunRepository for RecordingRepo {
    async fn insert(
        &self,
        run: &ReviewRun,
        _uow: &mut dyn UnitOfWork,
    ) -> Result<(), RepositoryError> {
        self.runs
            .lock()
            .map_err(|_| RepositoryError::Database("poisoned".to_owned()))?
            .push(run.clone());
        Ok(())
    }
    async fn list_for_user(&self, _u: UserId) -> Result<Vec<ReviewRun>, RepositoryError> {
        Ok(self
            .runs
            .lock()
            .map_err(|_| RepositoryError::Database("poisoned".to_owned()))?
            .clone())
    }
}

// ---------------------------------------------------------------------------
// Fixtures / builders
// ---------------------------------------------------------------------------

fn ticker(s: &str) -> Ticker {
    Ticker::try_new(s).unwrap()
}

fn aapl_position() -> Position {
    let now = Utc::now();
    Position {
        id: PositionId::generate(),
        user_id: UserId::generate(),
        ticker: ticker("AAPL"),
        account_label: "Brokerage".to_owned(),
        account_type: AccountType::Investment,
        shares: Decimal::new(10, 0),
        cost_basis: Some(Money::from_minor(150_000)),
        created_at: now,
        updated_at: now,
    }
}

fn rec(claims: Vec<Claim>, confidence: Confidence) -> Recommendation {
    Recommendation {
        title: "t".to_owned(),
        rationale: "r".to_owned(),
        confidence,
        claims,
    }
}

fn aapl_value_claim(cents: i64) -> Claim {
    Claim {
        subject: ClaimSubject::Position {
            ticker: ticker("AAPL"),
        },
        cited_value: Money::from_minor(cents),
        cited_percentage: None,
    }
}

fn output(recs: Vec<Recommendation>, finish: Option<&str>) -> AdvisorOutput {
    AdvisorOutput {
        recommendations: recs,
        raw_output: "{\"recommendations\":[]}".to_owned(),
        prompt_hash: "hash".to_owned(),
        prompt_tokens: Some(100),
        completion_tokens: Some(20),
        finish_reason: finish.map(str::to_owned),
    }
}

/// Build a use-case with the given positions/balances/market/advisor and a
/// recording repo handed back so the test can read the persisted run.
fn build(
    positions: Vec<Position>,
    balances: Vec<CashBalance>,
    market: FakeMarket,
    advisor: Arc<dyn InvestmentAdvisor>,
) -> (GeneratePortfolioReview, Arc<RecordingRepo>) {
    let repo = Arc::new(RecordingRepo::default());
    let uc = GeneratePortfolioReview::new(
        Arc::new(FakePositions(positions)),
        Arc::new(FakeBalances(balances)),
        Arc::new(market),
        advisor,
        Arc::clone(&repo) as Arc<dyn ReviewRunRepository>,
        Arc::new(FakeUowProvider),
    );
    (uc, repo)
}

fn market(price_cents: i64) -> FakeMarket {
    FakeMarket {
        price_cents,
        degrade: vec![],
    }
}

// ---------------------------------------------------------------------------
// Terminal states
// ---------------------------------------------------------------------------

#[tokio::test]
async fn empty_portfolio_short_circuits_without_a_model_call() {
    // PanicAdvisor proves no model call happens.
    let (uc, repo) = build(vec![], vec![], market(18_000), Arc::new(PanicAdvisor));
    let run = uc
        .generate_portfolio_review(UserId::generate(), Utc::now())
        .await
        .unwrap();
    assert_eq!(run.terminal_state, ReviewTerminalState::EmptyPortfolio);
    assert!(run.recommendations.is_empty());
    assert!(run.outcomes.is_empty());
    assert_eq!(run.latency_ms, 0);
    assert_eq!(run.prompt_tokens, None);
    // Persisted exactly once.
    assert_eq!(repo.runs.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn malformed_output_persists_with_raw_output() {
    let advisor = Arc::new(ScriptedAdvisor {
        result: Err(AdvisorError::Parse("garbage-from-model".to_owned())),
    });
    let (uc, repo) = build(vec![aapl_position()], vec![], market(18_000), advisor);
    let run = uc
        .generate_portfolio_review(UserId::generate(), Utc::now())
        .await
        .unwrap();
    assert_eq!(run.terminal_state, ReviewTerminalState::MalformedOutput);
    assert_eq!(run.raw_output, "garbage-from-model");
    assert!(!run.raw_output.is_empty());
    assert_eq!(repo.runs.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn other_advisor_error_is_transport_and_persists_nothing() {
    let advisor = Arc::new(ScriptedAdvisor {
        result: Err(AdvisorError::RateLimited("slow down".to_owned())),
    });
    let (uc, repo) = build(vec![aapl_position()], vec![], market(18_000), advisor);
    let out = uc
        .generate_portfolio_review(UserId::generate(), Utc::now())
        .await;
    assert!(matches!(
        out,
        Err(crate::error::ServiceError::AdvisorTransport(_))
    ));
    // No run persisted (retryable transport failure).
    assert!(repo.runs.lock().unwrap().is_empty());
}

#[tokio::test]
async fn zero_recommendations_is_no_verifiable_insights() {
    let advisor = Arc::new(ScriptedAdvisor {
        result: Ok(output(vec![], Some("STOP"))),
    });
    let (uc, _repo) = build(vec![aapl_position()], vec![], market(18_000), advisor);
    let run = uc
        .generate_portfolio_review(UserId::generate(), Utc::now())
        .await
        .unwrap();
    assert_eq!(
        run.terminal_state,
        ReviewTerminalState::NoVerifiableInsights
    );
}

#[tokio::test]
async fn all_unverified_recommendations_is_no_verifiable_insights() {
    // AAPL market_value is $1800 (10 x $180); cite $9999 -> every claim mismatches.
    let advisor = Arc::new(ScriptedAdvisor {
        result: Ok(output(
            vec![rec(vec![aapl_value_claim(999_900)], Confidence::Low)],
            Some("STOP"),
        )),
    });
    let (uc, _repo) = build(vec![aapl_position()], vec![], market(18_000), advisor);
    let run = uc
        .generate_portfolio_review(UserId::generate(), Utc::now())
        .await
        .unwrap();
    assert_eq!(
        run.terminal_state,
        ReviewTerminalState::NoVerifiableInsights
    );
    assert_eq!(run.outcomes.len(), 1);
    assert!(matches!(
        run.outcomes[0].1,
        ValidationOutcome::Unverified(_)
    ));
}

#[tokio::test]
async fn one_verified_recommendation_is_completed() {
    // AAPL market_value $1800; cite $1800 -> verified.
    let advisor = Arc::new(ScriptedAdvisor {
        result: Ok(output(
            vec![rec(vec![aapl_value_claim(180_000)], Confidence::High)],
            Some("STOP"),
        )),
    });
    let (uc, _repo) = build(vec![aapl_position()], vec![], market(18_000), advisor);
    let run = uc
        .generate_portfolio_review(UserId::generate(), Utc::now())
        .await
        .unwrap();
    assert_eq!(run.terminal_state, ReviewTerminalState::Completed);
    assert_eq!(run.outcomes[0].1, ValidationOutcome::Verified);
    // finish_reason flows through from the advisor output to the audit row.
    assert_eq!(run.finish_reason, Some("STOP".to_owned()));
    assert_eq!(run.prompt_tokens, Some(100));
    assert_eq!(run.completion_tokens, Some(20));
}

#[tokio::test]
async fn stale_quote_degrades_to_missing_market_data_by_index() {
    // The AAPL quote is degraded (Ok(None)); a claim citing AAPL must reconcile to
    // MissingMarketData. This read by index requires the paired outcomes shape
    // (§0.4). The run reaches a terminal state (NOT EmptyPortfolio).
    let market = FakeMarket {
        price_cents: 18_000,
        degrade: vec!["AAPL".to_owned()],
    };
    let advisor = Arc::new(ScriptedAdvisor {
        result: Ok(output(
            vec![rec(vec![aapl_value_claim(180_000)], Confidence::Medium)],
            Some("STOP"),
        )),
    });
    let repo = Arc::new(RecordingRepo::default());
    let uc = GeneratePortfolioReview::new(
        Arc::new(FakePositions(vec![aapl_position()])),
        Arc::new(FakeBalances(vec![])),
        Arc::new(market),
        advisor,
        Arc::clone(&repo) as Arc<dyn ReviewRunRepository>,
        Arc::new(FakeUowProvider),
    );
    let run = uc
        .generate_portfolio_review(UserId::generate(), Utc::now())
        .await
        .unwrap();
    assert_ne!(run.terminal_state, ReviewTerminalState::EmptyPortfolio);
    assert_eq!(
        run.outcomes[0],
        (
            0,
            ValidationOutcome::Unverified(UnverifiedReason::MissingMarketData("AAPL".to_owned()))
        )
    );
    // A degraded-only review has no verifiable insight.
    assert_eq!(
        run.terminal_state,
        ReviewTerminalState::NoVerifiableInsights
    );
}
