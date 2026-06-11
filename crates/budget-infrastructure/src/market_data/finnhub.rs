//! [`FinnhubMarketData`] — the real-time quote tier of the market-data chain
//! (`docs/AI_FEATURE_DESIGN.md §Phase 6`).
//!
//! Finnhub's `GET /api/v1/quote?symbol=<TICKER>` returns a small JSON object
//! whose `c` field is the current price. The API key is read from the
//! [`SecretVault`] (`BUDGET-PLAID-TOKEN-VAULT-1`: never config/env/logs) and sent
//! in the `X-Finnhub-Token` header. This is the OPTIONAL upgrade tier: when a key
//! is configured it provides real-time quotes; when it is not, the chain skips it
//! and Stooq + manual cover the feature (the real feature runs with NO API key).
//!
//! ## Quote shape (`ORCH-TRAINING-CUTOFF-1`: best-effort, confirm at smoke time)
//!
//! ```json
//! { "c": 180.25, "h": 182.0, "l": 179.5, "o": 180.0, "pc": 179.0, "t": 1718140801 }
//! ```
//! `c` is the resolved price. Finnhub returns `c: 0` for an unknown symbol (no
//! 404); a zero/absent `c` degrades to `Ok(None)` so the chain falls through.
//!
//! ## Provenance
//!
//! A resolved quote is tagged [`PriceProvenance::Market`] with source `"finnhub"`.
//! `as_of` is the `t` epoch-seconds field when present, else fetch time.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::Deserialize;

use budget_domain::auth::SecretVault;
use budget_domain::money::Money;
use budget_domain::portfolio::{
    MarketDataError, MarketDataProvider, PriceProvenance, PriceQuote, Ticker,
};

/// The vault secret name the Finnhub API key is read under
/// (`BUDGET-PLAID-TOKEN-VAULT-1`).
pub const FINNHUB_API_KEY_SECRET: &str = "finnhub-api-key";

/// The Finnhub source name recorded on a resolved quote's provenance.
pub const FINNHUB_SOURCE: &str = "finnhub";

/// The Finnhub quote endpoint base.
const FINNHUB_BASE: &str = "https://finnhub.io/api/v1/quote";

/// Finnhub's `/quote` response (only the fields we consume).
#[derive(Debug, Clone, Deserialize)]
struct FinnhubQuote {
    /// Current price (`c`). Finnhub returns `0` for an unknown symbol.
    #[serde(default)]
    c: f64,
    /// Quote timestamp, epoch seconds (`t`). `0`/absent => use fetch time.
    #[serde(default)]
    t: i64,
}

/// The Finnhub real-time quote [`MarketDataProvider`].
pub struct FinnhubMarketData {
    vault: Arc<dyn SecretVault>,
    http: reqwest::Client,
    base_url: String,
}

impl FinnhubMarketData {
    /// Build the source against the live Finnhub endpoint, reading the API key
    /// from `vault` per call.
    #[must_use]
    pub fn new(vault: Arc<dyn SecretVault>) -> Self {
        Self::with_base_url(vault, FINNHUB_BASE.to_owned())
    }

    /// Build against an explicit base URL (tests point this at a local fixture
    /// server; production uses [`FinnhubMarketData::new`]).
    #[must_use]
    pub fn with_base_url(vault: Arc<dyn SecretVault>, base_url: String) -> Self {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .unwrap_or_default();
        Self {
            vault,
            http,
            base_url,
        }
    }
}

#[async_trait]
impl MarketDataProvider for FinnhubMarketData {
    async fn quote(&self, ticker: &Ticker) -> Result<Option<PriceQuote>, MarketDataError> {
        let api_key = self
            .vault
            .get_secret(FINNHUB_API_KEY_SECRET)
            .await
            .map_err(|e| MarketDataError::SecretVault(e.to_string()))?;

        let url = format!(
            "{base}?symbol={symbol}",
            base = self.base_url,
            symbol = ticker.as_str(),
        );
        let response = self
            .http
            .get(&url)
            .header("X-Finnhub-Token", api_key.as_str())
            .send()
            .await
            .map_err(|e| MarketDataError::Api(format!("finnhub request failed: {e}")))?;
        let status = response.status();
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(MarketDataError::RateLimited(format!(
                "finnhub http {status}"
            )));
        }
        if !status.is_success() {
            return Err(MarketDataError::Api(format!("finnhub http {status}")));
        }
        let body = response
            .text()
            .await
            .map_err(|e| MarketDataError::Api(format!("finnhub body read failed: {e}")))?;

        parse_finnhub_quote(&body)
    }
}

/// Parse a Finnhub `/quote` body into a [`PriceQuote`] (or `None` to degrade).
///
/// Pure (no I/O) so it is unit-tested against captured payloads. A zero or
/// absent `c` is Finnhub's unknown-symbol marker → `None` (the chain falls
/// through). A decode failure is a typed [`MarketDataError::Api`].
///
/// # Errors
/// [`MarketDataError::Api`] if the body is not valid Finnhub-quote JSON.
fn parse_finnhub_quote(body: &str) -> Result<Option<PriceQuote>, MarketDataError> {
    let quote: FinnhubQuote = serde_json::from_str(body)
        .map_err(|e| MarketDataError::Api(format!("finnhub quote decode failed: {e}")))?;
    // c == 0 is Finnhub's "unknown symbol" sentinel; degrade rather than cite $0.
    if quote.c <= 0.0 {
        return Ok(None);
    }
    // The wire price is an f64 (Finnhub's shape); convert to exact Money via its
    // string form so no binary-float artifact enters the money domain
    // (BUDGET-MONEY-1: f64 is never the money TYPE, only the provider's wire unit).
    let price = Money::try_parse("finnhub_price", &quote.c.to_string())
        .map_err(|e| MarketDataError::Api(format!("finnhub price not representable: {e}")))?;
    let as_of: DateTime<Utc> = if quote.t > 0 {
        DateTime::from_timestamp(quote.t, 0).unwrap_or_else(Utc::now)
    } else {
        Utc::now()
    };
    Ok(Some(PriceQuote {
        price,
        provenance: PriceProvenance::Market {
            source: FINNHUB_SOURCE.to_owned(),
        },
        as_of,
    }))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn parses_current_price_from_a_captured_quote() {
        let body = r#"{"c":180.25,"h":182.0,"l":179.5,"o":180.0,"pc":179.0,"t":1718140801}"#;
        let quote = parse_finnhub_quote(body)
            .unwrap()
            .expect("a positive c resolves");
        assert_eq!(quote.price, Money::try_parse("x", "180.25").unwrap());
        assert_eq!(
            quote.provenance,
            PriceProvenance::Market {
                source: "finnhub".to_owned()
            }
        );
        // `t` is honored as the observation instant.
        assert_eq!(
            quote.as_of,
            DateTime::from_timestamp(1_718_140_801, 0).unwrap()
        );
    }

    #[test]
    fn zero_current_price_degrades_to_none() {
        // Finnhub returns c:0 for an unknown symbol (no 404).
        let body = r#"{"c":0,"h":0,"l":0,"o":0,"pc":0,"t":0}"#;
        assert_eq!(parse_finnhub_quote(body).unwrap(), None);
    }

    #[test]
    fn missing_timestamp_falls_back_to_fetch_time() {
        let body = r#"{"c":42.5}"#;
        let quote = parse_finnhub_quote(body)
            .unwrap()
            .expect("positive c resolves");
        assert_eq!(quote.price, Money::try_parse("x", "42.5").unwrap());
        // No `t`: as_of is "now"-ish (just assert it parsed without t).
    }

    #[test]
    fn garbage_body_is_a_typed_api_error() {
        let out = parse_finnhub_quote("not json");
        assert!(matches!(out, Err(MarketDataError::Api(_))));
    }
}
