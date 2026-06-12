//! `GeneratePortfolioReview` — the portfolio-review use-case (`docs/AI_FEATURE_DESIGN.md
//! §Phase 5`). Orchestrates ports only (`ARCH-STRICT-LAYERING-1`); the whole
//! reconciliation firewall is proven against the mock here, before any real
//! Gemini byte in Phase 6.
//!
//! ## The flow (§Phase 5)
//!
//! 1. Load positions + balances concurrently (`ARCH-PARALLEL-INDEPENDENT-1`).
//! 2. Both empty → persist an `EmptyPortfolio` run WITHOUT a model call; return.
//! 3. Resolve quotes concurrently + assemble the [`PortfolioSnapshot`] (via the
//!    shared [`assemble_snapshot`](crate::portfolio_snapshot::assemble_snapshot)).
//! 4. Call `advisor.recommend(&snapshot)`, latency-measured.
//!    - `Err(AdvisorError::Parse(raw))` → persist `MalformedOutput` with the raw
//!      output; return `Ok(run)`.
//!    - any other `Err` → `ServiceError::AdvisorTransport`; NO run persisted
//!      (retryable transport failure).
//! 5. `reconcile` each recommendation → `outcomes: Vec<(usize, ValidationOutcome)>`.
//! 6. Classify the terminal state (`Completed` / `NoVerifiableInsights`).
//! 7. Persist the `ReviewRun` in ONE unit of work (`ARCH-EXPLICIT-TX-1`).

pub mod reconcile;

use std::sync::Arc;
use std::time::Instant;

use chrono::{DateTime, Utc};

use budget_domain::ids::{ReviewRunId, UserId};
use budget_domain::portfolio::{
    AdvisorError, AdvisorOutput, CashBalanceSource, InvestmentAdvisor, MarketDataProvider,
    PortfolioSnapshot, PositionSource, ReviewRun, ReviewTerminalState, ValidationOutcome,
};
use budget_domain::repositories::ReviewRunRepository;
use budget_domain::uow::{UnitOfWork, UowProvider, UowProviderExt};

use crate::error::ServiceError;
use crate::portfolio_snapshot::assemble_snapshot;

use reconcile::reconcile;

/// The portfolio-review use-case (`§Phase 5`). Holds only ports.
pub struct GeneratePortfolioReview {
    positions: Arc<dyn PositionSource>,
    balances: Arc<dyn CashBalanceSource>,
    market: Arc<dyn MarketDataProvider>,
    advisor: Arc<dyn InvestmentAdvisor>,
    review_runs: Arc<dyn ReviewRunRepository>,
    uow: Arc<dyn UowProvider>,
    /// The DRIP catch-up engine (P7.4). When present, the grounding snapshot is
    /// assembled through it so the AI review reconciles against the ESTIMATED
    /// current shares WITH the provenance label (§2.5/§8, `BUDGET-AI-1`); when
    /// `None` the legacy `Uploaded`-only assembly runs (older tests / no DRIP).
    drip: Option<Arc<crate::portfolio_drip::DripCatchUpService>>,
}

impl GeneratePortfolioReview {
    /// Assemble the use-case from its ports (no DRIP — legacy `Uploaded`-only
    /// grounding). Use [`with_drip`](Self::with_drip) to wire the catch-up engine.
    #[must_use]
    pub fn new(
        positions: Arc<dyn PositionSource>,
        balances: Arc<dyn CashBalanceSource>,
        market: Arc<dyn MarketDataProvider>,
        advisor: Arc<dyn InvestmentAdvisor>,
        review_runs: Arc<dyn ReviewRunRepository>,
        uow: Arc<dyn UowProvider>,
    ) -> Self {
        Self {
            positions,
            balances,
            market,
            advisor,
            review_runs,
            uow,
            drip: None,
        }
    }

    /// Wire the DRIP catch-up engine onto the use-case (P7.4): the grounding
    /// snapshot then reflects each position's estimated current shares + its
    /// provenance label (§2.5/§8).
    #[must_use]
    pub fn with_drip(mut self, drip: Arc<crate::portfolio_drip::DripCatchUpService>) -> Self {
        self.drip = Some(drip);
        self
    }

    /// Generate (and persist) a portfolio review for `user_id` at `now`.
    ///
    /// Returns the persisted [`ReviewRun`] (even for `EmptyPortfolio` /
    /// `MalformedOutput` — the terminal state communicates the outcome). The only
    /// non-persisting outcome is a retryable advisor transport failure, which
    /// returns [`ServiceError::AdvisorTransport`].
    ///
    /// # Errors
    /// [`ServiceError::AdvisorTransport`] on a non-parse advisor failure;
    /// [`ServiceError::Domain`] on any persistence failure.
    pub async fn generate_portfolio_review(
        &self,
        user_id: UserId,
        now: DateTime<Utc>,
    ) -> Result<ReviewRun, ServiceError> {
        // 1. Concurrent independent loads (`ARCH-PARALLEL-INDEPENDENT-1`).
        let (positions, balances) = futures::try_join!(
            self.positions.positions_for_user(user_id),
            self.balances.balances_for_user(user_id),
        )?;

        // 2. Empty-portfolio short-circuit: NO model call.
        if positions.is_empty() && balances.is_empty() {
            let empty_snapshot =
                assemble_snapshot(user_id, vec![], vec![], self.market.as_ref(), now)
                    .await
                    .map_err(|e| ServiceError::AdvisorTransport(e.to_string()))?;
            let run = ReviewRun {
                id: ReviewRunId::generate(),
                user_id,
                model_id: self.advisor.model_id().to_owned(),
                prompt_hash: String::new(),
                raw_output: String::new(),
                snapshot: empty_snapshot,
                recommendations: vec![],
                outcomes: vec![],
                terminal_state: ReviewTerminalState::EmptyPortfolio,
                prompt_tokens: None,
                completion_tokens: None,
                finish_reason: None,
                latency_ms: 0,
                occurred_at: now,
            };
            return self.persist(run).await;
        }

        // 3. Concurrent quotes + snapshot assembly. When the DRIP engine is wired
        // (P7.4), assemble through it so the review reconciles against the
        // ESTIMATED current shares + provenance label (§2.5/§8); otherwise the
        // legacy `Uploaded`-only assembly.
        let snapshot = if let Some(drip) = &self.drip {
            crate::portfolio_snapshot::assemble_snapshot_with_drip(
                user_id,
                positions,
                balances,
                self.market.as_ref(),
                drip.as_ref(),
                now,
            )
            .await?
        } else {
            assemble_snapshot(user_id, positions, balances, self.market.as_ref(), now)
                .await
                .map_err(|e| ServiceError::AdvisorTransport(e.to_string()))?
        };

        // 4. The model call, latency-measured.
        let started = Instant::now();
        let recommend_result = self.advisor.recommend(&snapshot).await;
        let latency_ms = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);

        let output = match recommend_result {
            Ok(output) => output,
            Err(AdvisorError::Parse(raw)) => {
                // Parse failure: persist a MalformedOutput run carrying the raw
                // output for audit (FAILURE-of-review, still persisted).
                let run = malformed_run(user_id, &self.advisor, snapshot, raw, latency_ms, now);
                return self.persist(run).await;
            }
            // Any other advisor error is a retryable transport failure: NO run
            // persisted.
            Err(other) => return Err(ServiceError::AdvisorTransport(other.to_string())),
        };

        // 5. Reconcile each recommendation -> indexed outcomes.
        let outcomes: Vec<(usize, ValidationOutcome)> = output
            .recommendations
            .iter()
            .enumerate()
            .map(|(i, rec)| (i, reconcile(rec, &snapshot).outcome))
            .collect();

        // 6. Classify the terminal state.
        let terminal_state = classify(&output.recommendations, &outcomes);

        // 7. Persist in ONE unit of work.
        let run = completed_run(
            user_id,
            &self.advisor,
            snapshot,
            output,
            outcomes,
            terminal_state,
            latency_ms,
            now,
        );
        self.persist(run).await
    }

    /// Persist a [`ReviewRun`] inside ONE unit of work (`ARCH-EXPLICIT-TX-1`).
    async fn persist(&self, run: ReviewRun) -> Result<ReviewRun, ServiceError> {
        let review_runs = Arc::clone(&self.review_runs);
        let to_persist = run.clone();
        self.uow
            .run(move |uow: &dyn UnitOfWork| {
                Box::pin(async move {
                    // The locked `ReviewRunRepository::insert` takes `&mut dyn
                    // UnitOfWork`; the `UowProvider::run` closure yields a shared
                    // `&dyn UnitOfWork`. `ForwardingUow` bridges the two locked
                    // surfaces: it is an owned `dyn UnitOfWork` we can borrow
                    // mutably, and its `as_any` forwards to the inner handle so
                    // the infra downcast to the real transaction still succeeds.
                    let mut fwd = ForwardingUow(uow);
                    review_runs.insert(&to_persist, &mut fwd).await?;
                    Ok::<(), budget_domain::RepositoryError>(())
                })
            })
            .await?;
        Ok(run)
    }
}

/// Classify the terminal state of a completed model call (`§Phase 5` step 7):
/// zero recs OR zero verifiable → `NoVerifiableInsights`; else `Completed`.
fn classify(
    recommendations: &[budget_domain::portfolio::Recommendation],
    outcomes: &[(usize, ValidationOutcome)],
) -> ReviewTerminalState {
    let any_verifiable = outcomes
        .iter()
        .any(|(_, o)| matches!(o, ValidationOutcome::Verified));
    if recommendations.is_empty() || !any_verifiable {
        ReviewTerminalState::NoVerifiableInsights
    } else {
        ReviewTerminalState::Completed
    }
}

/// Build a `MalformedOutput` [`ReviewRun`] from a parse failure's raw output.
fn malformed_run(
    user_id: UserId,
    advisor: &Arc<dyn InvestmentAdvisor>,
    snapshot: PortfolioSnapshot,
    raw_output: String,
    latency_ms: i64,
    now: DateTime<Utc>,
) -> ReviewRun {
    ReviewRun {
        id: ReviewRunId::generate(),
        user_id,
        model_id: advisor.model_id().to_owned(),
        prompt_hash: String::new(),
        raw_output,
        snapshot,
        recommendations: vec![],
        outcomes: vec![],
        terminal_state: ReviewTerminalState::MalformedOutput,
        prompt_tokens: None,
        completion_tokens: None,
        finish_reason: None,
        latency_ms,
        occurred_at: now,
    }
}

/// Build a completed (or no-verifiable) [`ReviewRun`] from the model output.
#[allow(clippy::too_many_arguments)]
fn completed_run(
    user_id: UserId,
    advisor: &Arc<dyn InvestmentAdvisor>,
    snapshot: PortfolioSnapshot,
    output: AdvisorOutput,
    outcomes: Vec<(usize, ValidationOutcome)>,
    terminal_state: ReviewTerminalState,
    latency_ms: i64,
    now: DateTime<Utc>,
) -> ReviewRun {
    ReviewRun {
        id: ReviewRunId::generate(),
        user_id,
        model_id: advisor.model_id().to_owned(),
        prompt_hash: output.prompt_hash,
        raw_output: output.raw_output,
        snapshot,
        recommendations: output.recommendations,
        outcomes,
        terminal_state,
        prompt_tokens: output.prompt_tokens,
        completion_tokens: output.completion_tokens,
        finish_reason: output.finish_reason,
        latency_ms,
        occurred_at: now,
    }
}

/// An owned `dyn UnitOfWork` that forwards to a borrowed handle.
///
/// Bridges the two LOCKED surfaces: `ReviewRunRepository::insert` takes `&mut dyn
/// UnitOfWork`, while `UowProvider::run` yields a shared `&dyn UnitOfWork`. This
/// newtype is owned (so it can be borrowed mutably) and forwards [`as_any`] to
/// the inner handle, so an infrastructure repository's downcast to the concrete
/// transaction still recovers the real `SeaOrmUow` (the `&mut` is a signature
/// requirement, not a mutation need — `SeaOrmUow` is `Arc<Mutex<..>>`).
///
/// [`as_any`]: UnitOfWork::as_any
struct ForwardingUow<'a>(&'a dyn UnitOfWork);

impl UnitOfWork for ForwardingUow<'_> {
    fn as_any(&self) -> &dyn std::any::Any {
        self.0.as_any()
    }
}

#[cfg(test)]
mod tests;
