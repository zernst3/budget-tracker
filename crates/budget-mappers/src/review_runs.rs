//! Mapper: `budget-entities::review_runs::Model` ↔ `budget-domain::portfolio::ReviewRun`.
//!
//! `review_runs` is the append-only audit log for AI Portfolio Insights
//! (`docs/AI_FEATURE_DESIGN.md §0.4`, `SQL-AUDIT-COLUMNS-1`). This mapper bridges
//! the SeaORM `Model` (with three `JSONB` payload columns) and the rich domain
//! [`ReviewRun`] aggregate.
//!
//! ## The three JSONB payloads (the only non-trivial conversions)
//!
//! - `snapshot` — the grounding [`PortfolioSnapshot`], serde round-tripped.
//! - `outcomes` — the LOCKED index-paired shape `Vec<(usize, ValidationOutcome)>`
//!   (`§0.4`), serialized as a JSON array of two-element arrays
//!   (`[[0,{...}],[1,{...}]]`). The Phase-5 stale-quote terminal test reads
//!   `outcomes` by index, which requires exactly this paired shape.
//! - `recommendations` — the model's parsed `Vec<Recommendation>` (`§0.4`-addendum),
//!   so the audit row is self-contained (the UI renders cards without re-parsing
//!   `raw_output`).
//!
//! Each `JSONB` decode is FALLIBLE: a stored payload that fails to deserialize is
//! data corruption and surfaces as [`MapperError::InvalidStoredValue`]
//! (`DOMAIN-3` semantics extended to the JSONB boundary), never a panic.
//!
//! ## The terminal-state enum bridge (`§0.4` LOCKED)
//!
//! The entity enum is `ReviewTerminalStateEntity` (named `...Entity` to avoid a
//! collision with the domain [`ReviewTerminalState`] when both are imported here);
//! [`terminal_state_to_domain`] / [`terminal_state_to_entity`] convert 1:1.
//!
//! ## Audit columns
//!
//! `finish_reason` round-trips as `Option<String>` (audit-only — never surfaced in
//! a DTO, `§Phase 6`). `occurred_at` is the single timestamp (no `updated_at`,
//! append-only `SQL-AUDIT-COLUMNS-1`); converted `DateTimeWithTimeZone` →
//! `DateTime<Utc>` (`DOMAIN-7`).

use chrono::Utc;
use sea_orm::ActiveValue::Set;

use budget_domain::ids::{ReviewRunId, UserId};
use budget_domain::portfolio::{
    PortfolioSnapshot, Recommendation, ReviewRun, ReviewTerminalState, ValidationOutcome,
};

use budget_entities::review_runs;
use budget_entities::review_runs::ReviewTerminalStateEntity;

use crate::MapperError;

// ---------------------------------------------------------------------------
// Terminal-state enum bridge (§0.4 LOCKED)
// ---------------------------------------------------------------------------

/// Convert the entity terminal-state enum to the domain enum.
#[must_use]
pub fn terminal_state_to_domain(e: ReviewTerminalStateEntity) -> ReviewTerminalState {
    match e {
        ReviewTerminalStateEntity::Completed => ReviewTerminalState::Completed,
        ReviewTerminalStateEntity::NoVerifiableInsights => {
            ReviewTerminalState::NoVerifiableInsights
        }
        ReviewTerminalStateEntity::EmptyPortfolio => ReviewTerminalState::EmptyPortfolio,
        ReviewTerminalStateEntity::MalformedOutput => ReviewTerminalState::MalformedOutput,
    }
}

/// Convert the domain terminal-state enum to the entity enum.
#[must_use]
pub fn terminal_state_to_entity(d: ReviewTerminalState) -> ReviewTerminalStateEntity {
    match d {
        ReviewTerminalState::Completed => ReviewTerminalStateEntity::Completed,
        ReviewTerminalState::NoVerifiableInsights => {
            ReviewTerminalStateEntity::NoVerifiableInsights
        }
        ReviewTerminalState::EmptyPortfolio => ReviewTerminalStateEntity::EmptyPortfolio,
        ReviewTerminalState::MalformedOutput => ReviewTerminalStateEntity::MalformedOutput,
    }
}

// ---------------------------------------------------------------------------
// Public mapper functions
// ---------------------------------------------------------------------------

/// Decode a JSONB payload column into a domain type, mapping any serde failure to
/// the corruption error carrying the field name (`DOMAIN-3` at the JSONB boundary).
fn decode_jsonb<T: serde::de::DeserializeOwned>(
    value: sea_orm::JsonValue,
    field: &'static str,
) -> Result<T, MapperError> {
    serde_json::from_value(value).map_err(|e| MapperError::InvalidStoredValue {
        field,
        reason: e.to_string(),
    })
}

/// Translate a stored `review_runs` [`review_runs::Model`] into a domain
/// [`ReviewRun`].
///
/// FALLIBLE: each of the three JSONB payloads (`snapshot`, `outcomes`,
/// `recommendations`) is deserialized; a corrupt payload surfaces as
/// [`MapperError::InvalidStoredValue`] carrying the offending column.
///
/// # Errors
/// [`MapperError::InvalidStoredValue`] if any JSONB payload fails to deserialize
/// into its domain type.
pub fn model_to_domain(m: review_runs::Model) -> Result<ReviewRun, MapperError> {
    let snapshot: PortfolioSnapshot = decode_jsonb(m.snapshot, "snapshot")?;
    let outcomes: Vec<(usize, ValidationOutcome)> = decode_jsonb(m.outcomes, "outcomes")?;
    let recommendations: Vec<Recommendation> = decode_jsonb(m.recommendations, "recommendations")?;

    Ok(ReviewRun {
        id: ReviewRunId::new(m.id),
        user_id: UserId::new(m.user_id),
        model_id: m.model_id,
        prompt_hash: m.prompt_hash,
        raw_output: m.raw_output,
        snapshot,
        recommendations,
        outcomes,
        terminal_state: terminal_state_to_domain(m.terminal_state),
        prompt_tokens: m.prompt_tokens,
        completion_tokens: m.completion_tokens,
        finish_reason: m.finish_reason,
        latency_ms: m.latency_ms,
        occurred_at: m.occurred_at.with_timezone(&Utc),
    })
}

/// Encode a serializable domain payload into a JSONB value, mapping any serde
/// failure to the corruption error carrying the column name (symmetry with
/// [`decode_jsonb`]; a domain value that cannot serialize is a programming error,
/// surfaced rather than panicked, `SPIRIT-ROBUSTNESS-1`).
fn encode_jsonb<T: serde::Serialize>(
    value: &T,
    field: &'static str,
) -> Result<sea_orm::JsonValue, MapperError> {
    serde_json::to_value(value).map_err(|e| MapperError::InvalidStoredValue {
        field,
        reason: e.to_string(),
    })
}

/// Translate a domain [`ReviewRun`] into a `review_runs` [`review_runs::ActiveModel`].
///
/// FALLIBLE only on the JSONB serialize step (a domain value that cannot be
/// rendered to JSON is a programming error, not user data); surfaced as
/// [`MapperError::InvalidStoredValue`] for symmetry rather than a panic
/// (`SPIRIT-ROBUSTNESS-1`).
///
/// # Errors
/// [`MapperError::InvalidStoredValue`] if a JSONB payload fails to serialize.
pub fn domain_to_active_model(v: &ReviewRun) -> Result<review_runs::ActiveModel, MapperError> {
    let snapshot = encode_jsonb(&v.snapshot, "snapshot")?;
    let outcomes = encode_jsonb(&v.outcomes, "outcomes")?;
    let recommendations = encode_jsonb(&v.recommendations, "recommendations")?;

    Ok(review_runs::ActiveModel {
        id: Set(v.id.value()),
        user_id: Set(v.user_id.value()),
        model_id: Set(v.model_id.clone()),
        prompt_hash: Set(v.prompt_hash.clone()),
        raw_output: Set(v.raw_output.clone()),
        snapshot: Set(snapshot),
        outcomes: Set(outcomes),
        recommendations: Set(recommendations),
        terminal_state: Set(terminal_state_to_entity(v.terminal_state.clone())),
        prompt_tokens: Set(v.prompt_tokens),
        completion_tokens: Set(v.completion_tokens),
        finish_reason: Set(v.finish_reason.clone()),
        latency_ms: Set(v.latency_ms),
        occurred_at: Set(v.occurred_at.into()),
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use budget_domain::money::Money;
    use budget_domain::portfolio::{
        Claim, ClaimSubject, Confidence, NetWorth, Recommendation, Ticker, UnverifiedReason,
        ValidationOutcome,
    };
    use chrono::{TimeZone, Utc};
    use uuid::Uuid;

    fn sample_snapshot(user_id: Uuid) -> PortfolioSnapshot {
        PortfolioSnapshot {
            user_id: UserId::new(user_id),
            positions: vec![],
            cash_balances: vec![],
            buffer_total: Money::from_minor(500_000),
            net_worth: NetWorth {
                total_cash: Money::from_minor(600_000),
                total_positions: Money::from_minor(430_000),
                liabilities: Money::ZERO,
                total: Money::from_minor(1_030_000),
            },
            total_invested: Money::from_minor(430_000),
            captured_at: Utc.with_ymd_and_hms(2026, 6, 11, 12, 0, 0).unwrap(),
        }
    }

    fn sample_run(user_id: Uuid) -> ReviewRun {
        let rec = Recommendation {
            title: "Trim AAPL".to_owned(),
            rationale: "Concentration".to_owned(),
            confidence: Confidence::High,
            claims: vec![Claim {
                subject: ClaimSubject::Position {
                    ticker: Ticker::try_new("AAPL").unwrap(),
                },
                cited_value: Money::from_minor(180_000),
                cited_percentage: None,
            }],
        };
        ReviewRun {
            id: ReviewRunId::new(Uuid::new_v4()),
            user_id: UserId::new(user_id),
            model_id: "gemini-2.5-pro".to_owned(),
            prompt_hash: "abc123".to_owned(),
            raw_output: "{\"recommendations\":[]}".to_owned(),
            snapshot: sample_snapshot(user_id),
            recommendations: vec![rec],
            outcomes: vec![(
                0,
                ValidationOutcome::Unverified(UnverifiedReason::ValueMismatch {
                    cited: Money::from_minor(180_000),
                    ground_truth: Money::from_minor(180_001),
                }),
            )],
            terminal_state: ReviewTerminalState::Completed,
            prompt_tokens: Some(123),
            completion_tokens: Some(45),
            finish_reason: Some("STOP".to_owned()),
            latency_ms: 678,
            occurred_at: Utc.with_ymd_and_hms(2026, 6, 11, 12, 0, 1).unwrap(),
        }
    }

    /// Build a `Model` directly from a domain run by going domain -> active model
    /// -> a Model with the same JSONB payloads, so the round-trip exercises the
    /// real serde shapes (not a hand-written JSON blob that could drift).
    fn model_from(run: &ReviewRun) -> review_runs::Model {
        let am = domain_to_active_model(run).unwrap();
        // Pull each Set(..) value back out (mock-friendly: no DB).
        let get_json = |v: sea_orm::ActiveValue<sea_orm::JsonValue>| match v {
            Set(j) => j,
            _ => panic!("expected Set"),
        };
        review_runs::Model {
            id: run.id.value(),
            user_id: run.user_id.value(),
            model_id: run.model_id.clone(),
            prompt_hash: run.prompt_hash.clone(),
            raw_output: run.raw_output.clone(),
            snapshot: get_json(am.snapshot),
            outcomes: get_json(am.outcomes),
            recommendations: get_json(am.recommendations),
            terminal_state: terminal_state_to_entity(run.terminal_state.clone()),
            prompt_tokens: run.prompt_tokens,
            completion_tokens: run.completion_tokens,
            finish_reason: run.finish_reason.clone(),
            latency_ms: run.latency_ms,
            occurred_at: run.occurred_at.into(),
        }
    }

    #[test]
    fn review_run_round_trips_through_active_model_and_model() {
        let user_id = Uuid::new_v4();
        let run = sample_run(user_id);
        let model = model_from(&run);
        let back = model_to_domain(model).unwrap();
        assert_eq!(
            back, run,
            "the full ReviewRun survives the JSONB round-trip"
        );
    }

    #[test]
    fn outcomes_serialize_as_index_paired_array() {
        // §0.4 LOCKED: outcomes is `[[0,{...}],...]`, not a bare positional array.
        let run = sample_run(Uuid::new_v4());
        let am = domain_to_active_model(&run).unwrap();
        let outcomes_json = match am.outcomes {
            Set(j) => j,
            _ => panic!("expected Set"),
        };
        // Top-level is an array; first element is itself a 2-element array whose
        // head is the integer index 0.
        let arr = outcomes_json.as_array().expect("outcomes is a JSON array");
        let first = arr[0].as_array().expect("each outcome is a 2-tuple array");
        assert_eq!(
            first.len(),
            2,
            "index-paired tuple has exactly two elements"
        );
        assert_eq!(first[0].as_u64(), Some(0), "the head is the usize index");
    }

    #[test]
    fn terminal_state_enum_bridges_all_four_variants() {
        for d in [
            ReviewTerminalState::Completed,
            ReviewTerminalState::NoVerifiableInsights,
            ReviewTerminalState::EmptyPortfolio,
            ReviewTerminalState::MalformedOutput,
        ] {
            let e = terminal_state_to_entity(d.clone());
            assert_eq!(terminal_state_to_domain(e), d);
        }
    }

    #[test]
    fn corrupt_snapshot_jsonb_surfaces_as_invalid_stored_value() {
        let run = sample_run(Uuid::new_v4());
        let mut model = model_from(&run);
        // Replace the snapshot payload with something that cannot decode.
        model.snapshot = sea_orm::JsonValue::String("not a snapshot".to_owned());
        assert!(matches!(
            model_to_domain(model),
            Err(MapperError::InvalidStoredValue {
                field: "snapshot",
                ..
            })
        ));
    }
}
