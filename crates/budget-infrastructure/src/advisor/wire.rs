//! Gemini wire DTOs + the wire→domain boundary (`docs/AI_FEATURE_DESIGN.md
//! §Phase 4`, §0.3, §0.5).
//!
//! All Gemini-shaped serde structs live here, in the infrastructure crate — the
//! domain never sees an `http`/Gemini/`serde_json::Value` type (§0.3). These DTOs
//! are deliberately shared between two adapters:
//!
//! - the (Phase-6) real `GeminiAdvisor`, which deserializes the live Gemini HTTP
//!   response through them, and
//! - the [`MockInvestmentAdvisor`](super::mock::MockInvestmentAdvisor), which
//!   deserializes its *fixture* JSON through the SAME [`parse_advisor_response`]
//!   path (so the mock exercises the real byte-level JSON → domain contract, not
//!   just the Rust types — the firewall fidelity requirement).
//!
//! ## The structured-output shape (JSON-in-text)
//!
//! `wire.rs` assumes the Gemini structured output arrives as a JSON *string* in
//! `candidates[0].content.parts[0].text` (the JSON-in-text variant). The real
//! `responseSchema` wiring is an Open Item confirmed at Phase 6; if Gemini is
//! configured to return an inline structured object instead, only the `text`
//! extraction here changes — the domain mapping below does not.
//!
//! ## The one place the wire and domain vocabularies meet (§0.5)
//!
//! [`WireClaimSubject::kind`] is the snake_case discriminant string. The mapping
//! to [`ClaimSubject`] is pinned in [`wire_subject_to_domain`]:
//! `"position"`/`"buffer"`/`"net_worth"`/`"cost_basis_gain"`. A drift between the
//! fixture JSON, this constant, and the (Phase-6) `responseSchema` enum fails the
//! mock's round-trip test.

use std::str::FromStr;

use rust_decimal::Decimal;
use serde::Deserialize;

use budget_domain::money::Money;
use budget_domain::portfolio::{
    AdvisorError, AdvisorOutput, Claim, ClaimSubject, Confidence, Recommendation, Ticker,
};

// ===========================================================================
// Gemini response envelope (JSON-in-text)
// ===========================================================================

/// The top-level Gemini `generateContent` response.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct GeminiResponse {
    #[serde(default)]
    pub(crate) candidates: Vec<WireCandidate>,
}

/// One Gemini candidate.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct WireCandidate {
    pub(crate) content: WireContent,
    /// The model's stop reason (`"STOP"` / `"MAX_TOKENS"` / `"SAFETY"` …),
    /// carried through to [`AdvisorOutput::finish_reason`] for audit.
    #[serde(rename = "finishReason", default)]
    pub(crate) finish_reason: Option<String>,
    #[serde(rename = "usageMetadata", default)]
    pub(crate) usage_metadata: Option<WireUsageMetadata>,
}

/// A candidate's content (a list of parts).
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct WireContent {
    #[serde(default)]
    pub(crate) parts: Vec<WirePart>,
}

/// One content part. The structured output rides in `text` as a JSON string.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct WirePart {
    #[serde(default)]
    pub(crate) text: String,
}

/// Token-usage metadata (optional; absent on some responses / the mock).
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct WireUsageMetadata {
    #[serde(rename = "promptTokenCount", default)]
    pub(crate) prompt_token_count: Option<i64>,
    #[serde(rename = "candidatesTokenCount", default)]
    pub(crate) candidates_token_count: Option<i64>,
}

// ===========================================================================
// The structured recommendations payload (inside parts[0].text)
// ===========================================================================

/// The structured payload the model emits (the JSON inside `parts[0].text`).
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct WireRecommendations {
    #[serde(default)]
    pub(crate) recommendations: Vec<WireRecommendation>,
}

/// One model recommendation.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct WireRecommendation {
    pub(crate) title: String,
    pub(crate) rationale: String,
    /// Model self-reported confidence (`"high"`/`"medium"`/`"low"`); display-only
    /// (§added 2026-06-11), never reconciled.
    pub(crate) confidence: String,
    #[serde(default)]
    pub(crate) claims: Vec<WireClaim>,
}

/// One claim.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct WireClaim {
    pub(crate) subject: WireClaimSubject,
    pub(crate) cited_value: String,
    #[serde(default)]
    pub(crate) cited_percentage: Option<String>,
}

/// The claim subject discriminant (§0.5).
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct WireClaimSubject {
    #[serde(rename = "type")]
    pub(crate) kind: String,
    #[serde(default)]
    pub(crate) ticker: Option<String>,
}

// ===========================================================================
// wire -> domain
// ===========================================================================

/// Parse a deserialized Gemini response into the domain [`AdvisorOutput`]
/// (§Phase 4, §0.5).
///
/// Takes the first candidate, extracts token usage + `finish_reason`, reads
/// `parts[0].text` as the structured-JSON string, decodes it to
/// [`WireRecommendations`], and maps each recommendation/claim to the domain. The
/// raw `text` is preserved as [`AdvisorOutput::raw_output`]. `prompt_hash` is left
/// empty here (the real `GeminiAdvisor` fills it over the rendered prompt; the
/// mock stubs it).
///
/// # Errors
/// [`AdvisorError::Parse`] on any decode/validation failure (no candidate, no
/// part, malformed inner JSON, unknown subject kind, a `position`/`cost_basis_gain`
/// without a ticker, or an unparseable money/percentage value).
pub(crate) fn parse_advisor_response(wire: GeminiResponse) -> Result<AdvisorOutput, AdvisorError> {
    let candidate = wire
        .candidates
        .into_iter()
        .next()
        .ok_or_else(|| AdvisorError::Parse("gemini response had no candidates".to_owned()))?;

    let finish_reason = candidate.finish_reason.clone();
    let (prompt_tokens, completion_tokens) =
        candidate.usage_metadata.as_ref().map_or((None, None), |u| {
            (u.prompt_token_count, u.candidates_token_count)
        });

    let raw_output = candidate
        .content
        .parts
        .into_iter()
        .next()
        .map(|p| p.text)
        .ok_or_else(|| AdvisorError::Parse("gemini candidate had no content parts".to_owned()))?;

    let parsed: WireRecommendations = serde_json::from_str(&raw_output).map_err(|e| {
        AdvisorError::Parse(format!("structured-output JSON failed to decode: {e}"))
    })?;

    let recommendations = parsed
        .recommendations
        .into_iter()
        .map(wire_rec_to_domain)
        .collect::<Result<Vec<_>, _>>()?;

    Ok(AdvisorOutput {
        recommendations,
        raw_output,
        prompt_hash: String::new(),
        prompt_tokens,
        completion_tokens,
        finish_reason,
    })
}

/// Map one wire recommendation to the domain [`Recommendation`].
fn wire_rec_to_domain(w: WireRecommendation) -> Result<Recommendation, AdvisorError> {
    let confidence = wire_confidence_to_domain(&w.confidence)?;
    let claims = w
        .claims
        .into_iter()
        .map(wire_claim_to_domain)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Recommendation {
        title: w.title,
        rationale: w.rationale,
        confidence,
        claims,
    })
}

/// Map the wire confidence string (`"high"`/`"medium"`/`"low"`) to [`Confidence`].
fn wire_confidence_to_domain(raw: &str) -> Result<Confidence, AdvisorError> {
    match raw.trim().to_lowercase().as_str() {
        "high" => Ok(Confidence::High),
        "medium" => Ok(Confidence::Medium),
        "low" => Ok(Confidence::Low),
        other => Err(AdvisorError::Parse(format!(
            "unknown confidence '{other}' (expected high/medium/low)"
        ))),
    }
}

/// Map one wire claim to the domain [`Claim`].
fn wire_claim_to_domain(w: WireClaim) -> Result<Claim, AdvisorError> {
    let subject = wire_subject_to_domain(&w.subject)?;
    let cited_value = Money::try_parse("cited_value", &w.cited_value)
        .map_err(|e| AdvisorError::Parse(format!("cited_value parse failure: {e}")))?;
    let cited_percentage = match &w.cited_percentage {
        None => None,
        Some(s) if s.trim().is_empty() => None,
        Some(s) => Some(
            Decimal::from_str(s.trim())
                .map_err(|e| AdvisorError::Parse(format!("cited_percentage parse failure: {e}")))?,
        ),
    };
    Ok(Claim {
        subject,
        cited_value,
        cited_percentage,
    })
}

/// Map the wire subject discriminant to [`ClaimSubject`] (§0.5 — the single
/// place the wire and domain vocabularies meet).
fn wire_subject_to_domain(w: &WireClaimSubject) -> Result<ClaimSubject, AdvisorError> {
    let ticker = |raw: &Option<String>| -> Result<Ticker, AdvisorError> {
        let raw = raw.as_deref().ok_or_else(|| {
            AdvisorError::Parse(format!("subject kind '{}' requires a ticker", w.kind))
        })?;
        Ticker::try_new(raw)
            .map_err(|e| AdvisorError::Parse(format!("subject ticker invalid: {e}")))
    };
    match w.kind.as_str() {
        "position" => Ok(ClaimSubject::Position {
            ticker: ticker(&w.ticker)?,
        }),
        "buffer" => Ok(ClaimSubject::Buffer),
        "net_worth" => Ok(ClaimSubject::NetWorth),
        "cost_basis_gain" => Ok(ClaimSubject::CostBasisGain {
            ticker: ticker(&w.ticker)?,
        }),
        other => Err(AdvisorError::Parse(format!(
            "unknown claim subject kind '{other}' (expected \
             position/buffer/net_worth/cost_basis_gain)"
        ))),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    /// Wrap a structured-JSON payload string in a minimal Gemini envelope so the
    /// inner-JSON-in-text path is exercised.
    fn envelope(inner_json: &str, finish: &str) -> GeminiResponse {
        let escaped = serde_json::to_string(inner_json).unwrap();
        let full = format!(
            r#"{{"candidates":[{{"content":{{"parts":[{{"text":{escaped}}}]}},
               "finishReason":"{finish}",
               "usageMetadata":{{"promptTokenCount":11,"candidatesTokenCount":22}}}}]}}"#
        );
        serde_json::from_str(&full).unwrap()
    }

    #[test]
    fn parses_a_position_claim_with_percentage() {
        let inner = r#"{"recommendations":[
            {"title":"Trim AAPL","rationale":"big","confidence":"high",
             "claims":[{"subject":{"type":"position","ticker":"AAPL"},
                        "cited_value":"1800.00","cited_percentage":"0.4"}]}]}"#;
        let out = parse_advisor_response(envelope(inner, "STOP")).unwrap();
        assert_eq!(out.finish_reason, Some("STOP".to_owned()));
        assert_eq!(out.prompt_tokens, Some(11));
        assert_eq!(out.completion_tokens, Some(22));
        assert_eq!(out.recommendations.len(), 1);
        let claim = &out.recommendations[0].claims[0];
        assert_eq!(
            claim.subject,
            ClaimSubject::Position {
                ticker: Ticker::try_new("AAPL").unwrap()
            }
        );
        assert_eq!(claim.cited_value, Money::from_minor(180_000));
        assert_eq!(claim.cited_percentage, Some(Decimal::new(4, 1)));
        assert_eq!(out.recommendations[0].confidence, Confidence::High);
    }

    #[test]
    fn maps_all_four_subject_kinds() {
        let inner = r#"{"recommendations":[
            {"title":"t","rationale":"r","confidence":"medium","claims":[
              {"subject":{"type":"position","ticker":"AAPL"},"cited_value":"1"},
              {"subject":{"type":"buffer"},"cited_value":"2"},
              {"subject":{"type":"net_worth"},"cited_value":"3"},
              {"subject":{"type":"cost_basis_gain","ticker":"NVDA"},"cited_value":"4"}
            ]}]}"#;
        let out = parse_advisor_response(envelope(inner, "STOP")).unwrap();
        let kinds: Vec<&ClaimSubject> = out.recommendations[0]
            .claims
            .iter()
            .map(|c| &c.subject)
            .collect();
        assert!(matches!(kinds[0], ClaimSubject::Position { .. }));
        assert_eq!(kinds[1], &ClaimSubject::Buffer);
        assert_eq!(kinds[2], &ClaimSubject::NetWorth);
        assert!(matches!(kinds[3], ClaimSubject::CostBasisGain { .. }));
    }

    #[test]
    fn unknown_subject_kind_is_a_parse_error() {
        let inner = r#"{"recommendations":[
            {"title":"t","rationale":"r","confidence":"low",
             "claims":[{"subject":{"type":"moon_phase"},"cited_value":"1"}]}]}"#;
        assert!(matches!(
            parse_advisor_response(envelope(inner, "STOP")),
            Err(AdvisorError::Parse(_))
        ));
    }

    #[test]
    fn position_without_ticker_is_a_parse_error() {
        let inner = r#"{"recommendations":[
            {"title":"t","rationale":"r","confidence":"low",
             "claims":[{"subject":{"type":"position"},"cited_value":"1"}]}]}"#;
        assert!(matches!(
            parse_advisor_response(envelope(inner, "STOP")),
            Err(AdvisorError::Parse(_))
        ));
    }

    #[test]
    fn unknown_confidence_is_a_parse_error() {
        let inner = r#"{"recommendations":[
            {"title":"t","rationale":"r","confidence":"certain","claims":[]}]}"#;
        assert!(matches!(
            parse_advisor_response(envelope(inner, "STOP")),
            Err(AdvisorError::Parse(_))
        ));
    }

    #[test]
    fn malformed_inner_json_is_a_parse_error() {
        // The text is not valid JSON.
        let out = parse_advisor_response(envelope("not json at all", "STOP"));
        assert!(matches!(out, Err(AdvisorError::Parse(_))));
    }

    #[test]
    fn no_candidates_is_a_parse_error() {
        let resp: GeminiResponse = serde_json::from_str(r#"{"candidates":[]}"#).unwrap();
        assert!(matches!(
            parse_advisor_response(resp),
            Err(AdvisorError::Parse(_))
        ));
    }

    #[test]
    fn empty_recommendations_round_trip_succeeds() {
        let out = parse_advisor_response(envelope(r#"{"recommendations":[]}"#, "STOP")).unwrap();
        assert!(out.recommendations.is_empty());
    }
}
