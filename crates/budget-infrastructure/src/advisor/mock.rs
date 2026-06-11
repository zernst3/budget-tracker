//! [`MockInvestmentAdvisor`] — a fixture-driven [`InvestmentAdvisor`]
//! (`docs/AI_FEATURE_DESIGN.md §Phase 4`, mirrors `MockPlaidApi`).
//!
//! Each [`MockMode`] selects one captured Gemini-shaped fixture; `recommend`
//! deserializes it through the SAME [`parse_advisor_response`] path the real
//! (Phase-6) `GeminiAdvisor` uses, so the mock exercises the real byte-level
//! JSON → domain contract — the whole reconciliation firewall is proven against
//! this mock before a single real Gemini byte (the firewall note, §Phase 5).
//!
//! Per §0.1 the mock stubs `finish_reason: None` (it produced no live candidate)
//! and `prompt_hash: String::new()` (no rendered prompt) — it overrides whatever
//! the fixture's envelope carried after parsing, so those audit fields reflect
//! "mock origin", not the fixture's canned `STOP`.

use async_trait::async_trait;

use budget_domain::portfolio::{AdvisorError, AdvisorOutput, InvestmentAdvisor, PortfolioSnapshot};

use super::wire::{GeminiResponse, parse_advisor_response};

/// The model id the mock reports (`model_id()`), recorded on the audit row.
pub const MOCK_MODEL_ID: &str = "mock-gemini-portfolio-advisor";

const VERIFIED_JSON: &str = include_str!("fixtures/gemini_verified.json");
const HALLUCINATED_JSON: &str = include_str!("fixtures/gemini_hallucinated.json");
const EMPTY_RECS_JSON: &str = include_str!("fixtures/gemini_empty_recs.json");

/// Which captured fixture the [`MockInvestmentAdvisor`] replays.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum MockMode {
    /// All cited values match the canonical test snapshot (≥1 verifiable rec).
    #[default]
    Verified,
    /// Three deliberate fabrications (unknown ticker + two value mismatches) —
    /// the `BUDGET-AI-1` enforcement fixture.
    Hallucinated,
    /// Valid JSON, `recommendations: []` — drives `NoVerifiableInsights`.
    EmptyRecommendations,
}

/// A fixture-driven [`InvestmentAdvisor`] (no network).
#[derive(Debug, Clone, Copy)]
pub struct MockInvestmentAdvisor {
    mode: MockMode,
}

impl MockInvestmentAdvisor {
    /// Build a mock in the given [`MockMode`].
    #[must_use]
    pub const fn new(mode: MockMode) -> Self {
        Self { mode }
    }

    /// The default mock — [`MockMode::Verified`].
    #[must_use]
    pub const fn default_mock() -> Self {
        Self::new(MockMode::Verified)
    }

    /// The raw fixture JSON for this mock's mode.
    const fn fixture(self) -> &'static str {
        match self.mode {
            MockMode::Verified => VERIFIED_JSON,
            MockMode::Hallucinated => HALLUCINATED_JSON,
            MockMode::EmptyRecommendations => EMPTY_RECS_JSON,
        }
    }
}

#[async_trait]
impl InvestmentAdvisor for MockInvestmentAdvisor {
    async fn recommend(
        &self,
        _snapshot: &PortfolioSnapshot,
    ) -> Result<AdvisorOutput, AdvisorError> {
        // Deserialize the fixture envelope, then map through the SAME wire->domain
        // path the real adapter uses (firewall fidelity). An envelope that fails
        // to deserialize is a Parse error (a corrupt fixture surfaces loudly).
        let envelope: GeminiResponse = serde_json::from_str(self.fixture())
            .map_err(|e| AdvisorError::Parse(format!("mock fixture envelope decode: {e}")))?;
        let mut output = parse_advisor_response(envelope)?;
        // §0.1: the mock produced no live candidate, so the audit-only stop reason
        // and prompt hash reflect mock origin, not the fixture's canned values.
        output.finish_reason = None;
        output.prompt_hash = String::new();
        Ok(output)
    }

    fn model_id(&self) -> &str {
        MOCK_MODEL_ID
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use budget_domain::ids::UserId;
    use budget_domain::money::Money;
    use budget_domain::portfolio::{NetWorth, PortfolioSnapshot};
    use chrono::Utc;

    fn empty_snapshot() -> PortfolioSnapshot {
        PortfolioSnapshot {
            user_id: UserId::generate(),
            positions: vec![],
            cash_balances: vec![],
            buffer_total: Money::ZERO,
            net_worth: NetWorth {
                total_cash: Money::ZERO,
                total_positions: Money::ZERO,
                liabilities: Money::ZERO,
                total: Money::ZERO,
            },
            total_invested: Money::ZERO,
            captured_at: Utc::now(),
        }
    }

    #[tokio::test]
    async fn verified_fixture_round_trips_with_at_least_one_rec() {
        let advisor = MockInvestmentAdvisor::default_mock();
        let out = advisor.recommend(&empty_snapshot()).await.unwrap();
        assert!(!out.recommendations.is_empty());
        // §0.1: the mock stubs finish_reason None + prompt_hash empty.
        assert_eq!(out.finish_reason, None);
        assert!(out.prompt_hash.is_empty());
        // The raw output is preserved for the audit row.
        assert!(!out.raw_output.is_empty());
    }

    #[tokio::test]
    async fn hallucinated_fixture_yields_exactly_three_recs() {
        let advisor = MockInvestmentAdvisor::new(MockMode::Hallucinated);
        let out = advisor.recommend(&empty_snapshot()).await.unwrap();
        assert_eq!(out.recommendations.len(), 3);
    }

    #[tokio::test]
    async fn empty_recs_fixture_yields_zero_recs() {
        let advisor = MockInvestmentAdvisor::new(MockMode::EmptyRecommendations);
        let out = advisor.recommend(&empty_snapshot()).await.unwrap();
        assert!(out.recommendations.is_empty());
    }

    #[test]
    fn model_id_is_the_mock_constant() {
        assert_eq!(
            MockInvestmentAdvisor::default_mock().model_id(),
            MOCK_MODEL_ID
        );
    }
}
