//! The real Gemini [`InvestmentAdvisor`] HTTP adapter (`docs/AI_FEATURE_DESIGN.md
//! §Phase 6`).
//!
//! Builds the grounding prompt from a [`PortfolioSnapshot`], calls Google's
//! Generative Language `generateContent` endpoint with `responseMimeType:
//! application/json` + a `responseSchema` mirroring the `Claim`/`ClaimSubject`
//! shape (§0.5), computes `prompt_hash = sha256(rendered_prompt)`, and parses the
//! response through the SAME [`parse_advisor_response`] path the mock uses (so the
//! firewall contract is identical). A parse failure is [`AdvisorError::Parse`]
//! carrying the raw output.
//!
//! ## Provider note
//!
//! This adapter targets **Google Gemini** (the `generativelanguage.googleapis.com`
//! REST API), NOT an Anthropic/Claude model — the wire shape here
//! (`generationConfig.responseSchema`, `candidates[].finishReason`,
//! `parts[0].text`) is Google's. The model id is config-resolved
//! (`ORCH-TRAINING-CUTOFF-1`); no model id, endpoint, or rate limit is hardcoded
//! as fact beyond the seeded-default config string the caller passes in.
//!
//! ## Secret handling (`BUDGET-PLAID-TOKEN-VAULT-1`)
//!
//! The API key is read from the [`SecretVault`] at call time (never from
//! config/env/logs) under the secret name [`GEMINI_API_KEY_SECRET`]. It is placed
//! ONLY in the `x-goog-api-key` request header sent to Google; it is never
//! logged, never put in an error payload, and never stored. [`AdvisorError`]
//! carries only
//! `String` descriptions of the failure category (§0.3), never the key or a raw
//! Gemini error object.
//!
//! ## Verification boundary
//!
//! The live network path (real key + real Google endpoint) cannot be exercised in
//! CI. It is covered indirectly: the parse path is tested against a captured
//! real-shaped fixture, the `prompt_hash` is tested for determinism, and the
//! whole reconciliation firewall is proven against the mock (§Phase 5). The true
//! live smoke test is a human step.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::Serialize;
use serde_json::json;
use sha2::{Digest, Sha256};

use budget_domain::auth::SecretVault;
use budget_domain::portfolio::{
    AdvisorError, AdvisorOutput, InvestmentAdvisor, PortfolioSnapshot, PriceProvenance,
};

use super::wire::{GeminiResponse, parse_advisor_response};

/// The vault secret name the Gemini API key is read under
/// (`BUDGET-PLAID-TOKEN-VAULT-1`: the key lives ONLY in the vault, never in
/// config/env/logs). The vault maps this name to the live key.
pub const GEMINI_API_KEY_SECRET: &str = "gemini-api-key";

/// The Generative Language API base (the `v1beta` `generateContent` surface).
/// Not a model id or a rate limit — the endpoint shape itself
/// (`ORCH-TRAINING-CUTOFF-1`: confirm at build/smoke time, treat as best-effort).
const GEMINI_API_BASE: &str = "https://generativelanguage.googleapis.com/v1beta/models";

/// The real Gemini-backed [`InvestmentAdvisor`].
///
/// Holds the secret vault (for the API key, resolved per call) and the
/// config-resolved `model_id` (never hardcoded — `ORCH-TRAINING-CUTOFF-1`).
pub struct GeminiAdvisor {
    vault: Arc<dyn SecretVault>,
    model_id: String,
    http: reqwest::Client,
}

impl GeminiAdvisor {
    /// Build the advisor from the secret vault and the config-resolved `model_id`.
    ///
    /// `model_id` must come from configuration (`GEMINI_MODEL_IDS` allow-list,
    /// validated by the caller) — never a hardcoded literal
    /// (`ORCH-TRAINING-CUTOFF-1`).
    #[must_use]
    pub fn new(vault: Arc<dyn SecretVault>, model_id: String) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .unwrap_or_default();
        Self {
            vault,
            model_id,
            http,
        }
    }

    /// The JSON `responseSchema` the model must conform to (§0.5). Identical for
    /// every model. The subject `type` enum is EXACTLY
    /// `["position","buffer","net_worth","cost_basis_gain"]` and the per-rec
    /// `confidence` enum is EXACTLY `["high","medium","low"]` — these strings are
    /// shared with `wire.rs`'s mapping and the fixtures; a drift fails the mock's
    /// round-trip test.
    ///
    /// Money/percentage cross as STRINGS (parsed to `Money`/`Decimal` in
    /// `wire.rs`, `BUDGET-MONEY-1`); `cited_percentage` is a RATIO in `[0,1]`
    /// (the schema description instructs the model accordingly, §0.5).
    #[must_use]
    fn response_schema() -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "recommendations": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "title": { "type": "string" },
                            "rationale": { "type": "string" },
                            "confidence": {
                                "type": "string",
                                "enum": ["high", "medium", "low"]
                            },
                            "claims": {
                                "type": "array",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "subject": {
                                            "type": "object",
                                            "properties": {
                                                "type": {
                                                    "type": "string",
                                                    "enum": [
                                                        "position",
                                                        "buffer",
                                                        "net_worth",
                                                        "cost_basis_gain"
                                                    ]
                                                },
                                                "ticker": { "type": "string" }
                                            },
                                            "required": ["type"]
                                        },
                                        "cited_value": {
                                            "type": "string",
                                            "description": "An exact decimal money string, e.g. \"1800.00\". Never a number, never a currency symbol."
                                        },
                                        "cited_percentage": {
                                            "type": "string",
                                            "description": "Optional. The position's share of the invested total as a RATIO in [0,1] (e.g. \"0.42\" for 42%), NOT a 0-100 percentage. Only valid on a `position` subject."
                                        }
                                    },
                                    "required": ["subject", "cited_value"]
                                }
                            }
                        },
                        "required": ["title", "rationale", "confidence", "claims"]
                    }
                }
            },
            "required": ["recommendations"]
        })
    }

    /// Render the grounding prompt from the snapshot. The exact text is hashed to
    /// `prompt_hash`, so it must be DETERMINISTIC for a fixed snapshot (no
    /// timestamps-of-now, no map iteration order). Positions render in their
    /// stored order; figures are exact decimal strings (`BUDGET-MONEY-1`).
    #[must_use]
    fn render_prompt(&self, snapshot: &PortfolioSnapshot) -> String {
        let mut prompt = String::new();
        prompt.push_str(
            "You are a portfolio-review assistant. You are given a grounded \
             snapshot of a user's investment portfolio. Produce concise, \
             actionable recommendations. Every numeric claim you make MUST cite a \
             figure that appears in the snapshot below, as an exact decimal money \
             string. Allowed claim subjects: a position's market value \
             (`position` + ticker), the reserved cash buffer (`buffer`), total \
             net worth (`net_worth`), or a position's unrealized gain \
             (`cost_basis_gain` + ticker). For a `position` claim you MAY also \
             cite `cited_percentage` as the position's share of the invested \
             total, expressed as a RATIO in [0,1]. Do not invent tickers or \
             figures.\n\n",
        );

        prompt.push_str("=== PORTFOLIO SNAPSHOT ===\n");
        prompt.push_str(&format!(
            "captured_at: {}\n",
            snapshot.captured_at.to_rfc3339()
        ));
        prompt.push_str(&format!(
            "net_worth.total: {}\n",
            snapshot.net_worth.total.as_decimal()
        ));
        prompt.push_str(&format!(
            "total_invested: {}\n",
            snapshot.total_invested.as_decimal()
        ));
        prompt.push_str(&format!(
            "reserved_buffer_total: {}\n",
            snapshot.buffer_total.as_decimal()
        ));

        prompt.push_str("\n-- positions --\n");
        for pp in &snapshot.positions {
            let market_value = pp
                .market_value
                .map_or_else(|| "UNRESOLVED".to_owned(), |m| m.as_decimal().to_string());
            let cost_basis = pp
                .position
                .cost_basis
                .map_or_else(|| "unknown".to_owned(), |m| m.as_decimal().to_string());
            let source = match pp.quote.as_ref().map(|q| &q.provenance) {
                Some(PriceProvenance::Market { source }) => source.clone(),
                Some(PriceProvenance::Manual) => "manual".to_owned(),
                None => "none".to_owned(),
            };
            prompt.push_str(&format!(
                "  {ticker}: account={account} shares={shares} \
                 market_value={market_value} cost_basis={cost_basis} \
                 price_source={source}\n",
                ticker = pp.position.ticker.as_str(),
                account = pp.position.account_label,
                shares = pp.position.shares,
            ));
        }

        prompt.push_str("\n-- cash balances --\n");
        for cb in &snapshot.cash_balances {
            prompt.push_str(&format!(
                "  {label}: balance={balance} reserved={reserved}\n",
                label = cb.account_label,
                balance = cb.balance.as_decimal(),
                reserved = cb.reserved,
            ));
        }

        prompt
    }
}

/// The Gemini `generateContent` request body (the wire OUT shape).
///
/// Lives here (not in `wire.rs`, which holds the response/IN shapes) because it
/// is request-only. `generationConfig` carries the structured-output config
/// (§Phase 6): `responseMimeType: application/json` + the `responseSchema`.
#[derive(Debug, Serialize)]
struct GeminiRequest {
    contents: Vec<RequestContent>,
    #[serde(rename = "generationConfig")]
    generation_config: GenerationConfig,
}

#[derive(Debug, Serialize)]
struct RequestContent {
    parts: Vec<RequestPart>,
}

#[derive(Debug, Serialize)]
struct RequestPart {
    text: String,
}

#[derive(Debug, Serialize)]
struct GenerationConfig {
    #[serde(rename = "responseMimeType")]
    response_mime_type: String,
    #[serde(rename = "responseSchema")]
    response_schema: serde_json::Value,
}

#[async_trait]
impl InvestmentAdvisor for GeminiAdvisor {
    async fn recommend(&self, snapshot: &PortfolioSnapshot) -> Result<AdvisorOutput, AdvisorError> {
        // 1. Resolve the API key from the vault (never logged; §0.3).
        let api_key = self
            .vault
            .get_secret(GEMINI_API_KEY_SECRET)
            .await
            .map_err(|e| AdvisorError::SecretVault(e.to_string()))?;

        // 2. Render the prompt and hash it (over the EXACT bytes sent).
        let rendered_prompt = self.render_prompt(snapshot);
        let prompt_hash = sha256_hex(&rendered_prompt);

        // 3. Build the structured-output request.
        let body = GeminiRequest {
            contents: vec![RequestContent {
                parts: vec![RequestPart {
                    text: rendered_prompt,
                }],
            }],
            generation_config: GenerationConfig {
                response_mime_type: "application/json".to_owned(),
                response_schema: Self::response_schema(),
            },
        };

        // 4. Call Gemini. The key rides only in the request header (never in a
        // log; `x-goog-api-key` is the header form, avoiding it landing in any
        // URL-logging middleware).
        let url = format!(
            "{base}/{model}:generateContent",
            base = GEMINI_API_BASE,
            model = self.model_id,
        );
        let response = self
            .http
            .post(&url)
            .header("x-goog-api-key", api_key.as_str())
            .json(&body)
            .send()
            .await
            .map_err(|e| AdvisorError::Unavailable(redact(&e.to_string(), &api_key)))?;

        // 5. Map HTTP status to a typed error (no key, no raw Gemini error body).
        let status = response.status();
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(AdvisorError::RateLimited(format!("gemini http {status}")));
        }
        if !status.is_success() {
            return Err(AdvisorError::Api(format!("gemini http {status}")));
        }

        // 6. Deserialize the envelope, then map through the SAME wire->domain path
        // the mock uses. A decode/parse failure is a Parse error carrying the raw
        // body (the use-case persists it as MalformedOutput).
        let raw_body = response
            .text()
            .await
            .map_err(|e| AdvisorError::Api(redact(&e.to_string(), &api_key)))?;
        let envelope: GeminiResponse = serde_json::from_str(&raw_body).map_err(|e| {
            AdvisorError::Parse(format!("gemini response envelope decode failed: {e}"))
        })?;
        let mut output = parse_advisor_response(envelope)?;

        // 7. The real adapter fills prompt_hash over the rendered prompt (the mock
        // stubs it empty).
        output.prompt_hash = prompt_hash;
        Ok(output)
    }

    fn model_id(&self) -> &str {
        &self.model_id
    }
}

/// Hex-encoded SHA-256 of the rendered prompt (`prompt_hash`).
#[must_use]
fn sha256_hex(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    hex::encode(hasher.finalize())
}

/// Defensively strip the API key from any error string before it can be logged
/// or returned (`BUDGET-PLAID-TOKEN-VAULT-1`: the key never reaches a log/error).
/// `reqwest` errors should not contain the key (it rides in a header, not the
/// URL), but a redaction pass is cheap insurance against any future carrier.
#[must_use]
fn redact(message: &str, secret: &str) -> String {
    if secret.is_empty() {
        return message.to_owned();
    }
    message.replace(secret, "***")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use budget_domain::ids::{PositionId, UserId};
    use budget_domain::money::Money;
    use budget_domain::portfolio::{
        ClaimSubject, NetWorth, Position, PriceQuote, PricedPosition, Ticker,
    };
    use chrono::{TimeZone, Utc};

    /// A captured-shape Gemini `generateContent` response (the JSON-in-text
    /// variant): the structured payload rides as a JSON string in
    /// `candidates[0].content.parts[0].text`, alongside `finishReason` +
    /// `usageMetadata`. This is the real wire shape the parse path must handle.
    const CAPTURED_RESPONSE: &str = include_str!("fixtures/gemini_captured_response.json");

    fn fixed_snapshot() -> PortfolioSnapshot {
        let captured_at = Utc.with_ymd_and_hms(2026, 6, 11, 12, 0, 0).unwrap();
        let position = Position {
            id: PositionId::new(
                uuid::Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap(),
            ),
            user_id: UserId::new(
                uuid::Uuid::parse_str("00000000-0000-0000-0000-000000000002").unwrap(),
            ),
            ticker: Ticker::try_new("AAPL").unwrap(),
            account_label: "Brokerage".to_owned(),
            account_type: budget_domain::enums::AccountType::Investment,
            shares: rust_decimal::Decimal::new(10, 0),
            cost_basis: Some(Money::from_minor(150_000)),
            created_at: captured_at,
            updated_at: captured_at,
        };
        let priced = PricedPosition {
            position,
            quote: Some(PriceQuote {
                price: Money::from_minor(18_000),
                provenance: PriceProvenance::Market {
                    source: "finnhub".to_owned(),
                },
                as_of: captured_at,
            }),
            market_value: Some(Money::from_minor(180_000)),
        };
        PortfolioSnapshot {
            user_id: UserId::new(
                uuid::Uuid::parse_str("00000000-0000-0000-0000-000000000002").unwrap(),
            ),
            positions: vec![priced],
            cash_balances: vec![],
            buffer_total: Money::from_minor(500_000),
            net_worth: NetWorth {
                total_cash: Money::from_minor(600_000),
                total_positions: Money::from_minor(180_000),
                liabilities: Money::ZERO,
                total: Money::from_minor(780_000),
            },
            total_invested: Money::from_minor(180_000),
            captured_at,
        }
    }

    #[test]
    fn parses_a_captured_real_shaped_response() {
        // The fixture decodes through the same GeminiResponse + parse_advisor_response
        // path the live adapter uses (ORCH-NEW-PATH-TESTS-1).
        let envelope: GeminiResponse = serde_json::from_str(CAPTURED_RESPONSE).unwrap();
        let out = parse_advisor_response(envelope).unwrap();
        assert_eq!(out.finish_reason, Some("STOP".to_owned()));
        assert_eq!(out.prompt_tokens, Some(742));
        assert_eq!(out.completion_tokens, Some(118));
        assert_eq!(out.recommendations.len(), 1);
        let claim = &out.recommendations[0].claims[0];
        assert_eq!(
            claim.subject,
            ClaimSubject::Position {
                ticker: Ticker::try_new("AAPL").unwrap()
            }
        );
        assert_eq!(claim.cited_value, Money::from_minor(180_000));
    }

    #[test]
    fn prompt_hash_is_deterministic_for_a_fixed_prompt() {
        // The hash is a pure function of the prompt bytes: same input -> same hash,
        // and it is the sha256 hex of those exact bytes.
        let fixed = "the exact rendered prompt bytes";
        let a = sha256_hex(fixed);
        let b = sha256_hex(fixed);
        assert_eq!(a, b, "sha256 is deterministic");
        assert_eq!(a.len(), 64, "sha256 hex is 64 chars");
        // Known-answer: sha256("") is the well-known empty digest.
        assert_eq!(
            sha256_hex(""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn render_prompt_is_deterministic_for_a_fixed_snapshot() {
        // The advisor renders a stable prompt (no now(), no map iteration), so the
        // prompt_hash is reproducible across calls with the same snapshot.
        let vault: Arc<dyn SecretVault> = Arc::new(crate::plaid::InMemorySecretVault::new());
        let advisor = GeminiAdvisor::new(vault, "gemini-2.5-pro".to_owned());
        let snap = fixed_snapshot();
        let a = advisor.render_prompt(&snap);
        let b = advisor.render_prompt(&snap);
        assert_eq!(a, b);
        // The cited ground-truth figures appear verbatim in the prompt so the
        // model can cite them.
        assert!(a.contains("AAPL"));
        assert!(a.contains("1800.00"), "AAPL market value is in the prompt");
        assert!(
            a.contains("5000.00"),
            "the reserved buffer is in the prompt"
        );
    }

    #[test]
    fn response_schema_pins_the_subject_and_confidence_enums() {
        // §0.5: the subject `type` enum and the confidence enum are the single
        // place the wire and domain vocabularies meet; a drift here would silently
        // diverge from wire.rs's mapping.
        let schema = GeminiAdvisor::response_schema();
        let subject_enum = &schema["properties"]["recommendations"]["items"]["properties"]["claims"]
            ["items"]["properties"]["subject"]["properties"]["type"]["enum"];
        assert_eq!(
            subject_enum,
            &serde_json::json!(["position", "buffer", "net_worth", "cost_basis_gain"])
        );
        let confidence_enum =
            &schema["properties"]["recommendations"]["items"]["properties"]["confidence"]["enum"];
        assert_eq!(
            confidence_enum,
            &serde_json::json!(["high", "medium", "low"])
        );
    }

    #[test]
    fn redact_strips_the_key_from_an_error_string() {
        let msg = "request to ...?key=SECRET123 failed";
        assert_eq!(redact(msg, "SECRET123"), "request to ...?key=*** failed");
        // An empty secret is a no-op (the vault could not be reached at all).
        assert_eq!(redact(msg, ""), msg);
    }
}
