//! [`StooqMarketData`] — a keyless CSV market-data source
//! (`docs/AI_FEATURE_DESIGN.md §Phase 6`, the market-data fallback chain).
//!
//! Stooq exposes a keyless quote CSV at
//! `https://stooq.com/q/l/?s=<ticker>.us&f=sd2t2ohlcv&h&e=csv`. Because it needs
//! no API key, it is the chain tier that lets the real feature run with NO
//! configured key at all (Finnhub upgrades to real-time quotes when a key is
//! present; Stooq + manual cover everything else).
//!
//! ## CSV shape (`ORCH-TRAINING-CUTOFF-1`: best-effort, confirm at smoke time)
//!
//! With `f=sd2t2ohlcv` + header (`h`), the response is two lines:
//! ```text
//! Symbol,Date,Time,Open,High,Low,Close,Volume
//! AAPL.US,2026-06-11,22:00:01,180.0,182.0,179.5,180.0,1234567
//! ```
//! The `Close` column is the resolved price. A `N/D` close (Stooq's "no data"
//! marker) or a missing/short row degrades to `Ok(None)` (the chain falls through
//! to the next tier).
//!
//! ## Provenance
//!
//! A resolved quote is tagged [`PriceProvenance::Market`] with source `"stooq"`
//! and `as_of = now` (Stooq's row carries a date/time, but the v1 contract stamps
//! observation time at fetch; the parsed date is not yet threaded through).

use async_trait::async_trait;

use budget_domain::money::Money;
use budget_domain::portfolio::{
    MarketDataError, MarketDataProvider, PriceProvenance, PriceQuote, Ticker,
};

/// The Stooq source name recorded on a resolved quote's provenance.
pub const STOOQ_SOURCE: &str = "stooq";

/// The keyless Stooq quote base URL (the `l`ight quote endpoint).
const STOOQ_BASE: &str = "https://stooq.com/q/l/";

/// A keyless Stooq CSV [`MarketDataProvider`].
pub struct StooqMarketData {
    http: reqwest::Client,
    base_url: String,
}

impl Default for StooqMarketData {
    fn default() -> Self {
        Self::new()
    }
}

impl StooqMarketData {
    /// Build the source against the live Stooq endpoint.
    #[must_use]
    pub fn new() -> Self {
        Self::with_base_url(STOOQ_BASE.to_owned())
    }

    /// Build against an explicit base URL (tests point this at a local fixture
    /// server; production uses [`StooqMarketData::new`]).
    #[must_use]
    pub fn with_base_url(base_url: String) -> Self {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .unwrap_or_default();
        Self { http, base_url }
    }

    /// The Stooq symbol form: lowercased ticker + the `.us` market suffix.
    #[must_use]
    fn stooq_symbol(ticker: &Ticker) -> String {
        format!("{}.us", ticker.as_str().to_lowercase())
    }
}

#[async_trait]
impl MarketDataProvider for StooqMarketData {
    async fn quote(&self, ticker: &Ticker) -> Result<Option<PriceQuote>, MarketDataError> {
        let symbol = Self::stooq_symbol(ticker);
        let url = format!(
            "{base}?s={symbol}&f=sd2t2ohlcv&h&e=csv",
            base = self.base_url
        );

        let response = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| MarketDataError::Api(format!("stooq request failed: {e}")))?;
        if !response.status().is_success() {
            return Err(MarketDataError::Api(format!(
                "stooq http {}",
                response.status()
            )));
        }
        let csv = response
            .text()
            .await
            .map_err(|e| MarketDataError::Api(format!("stooq body read failed: {e}")))?;

        Ok(parse_stooq_csv(&csv))
    }
}

/// Parse the Stooq quote CSV, returning the close price (or `None` to degrade).
///
/// Pure (no I/O) so it is unit-tested against captured payloads. Returns `None`
/// on any shape it cannot read (missing data line, short row, `N/D` close,
/// unparseable close) — the chain then falls through to the next tier.
#[must_use]
fn parse_stooq_csv(csv: &str) -> Option<PriceQuote> {
    // Line 0 is the header; line 1 is the data row.
    let data_line = csv.lines().nth(1)?;
    let fields: Vec<&str> = data_line.split(',').collect();
    // Symbol,Date,Time,Open,High,Low,Close,Volume — Close is index 6.
    let close = fields.get(6)?.trim();
    if close.is_empty() || close.eq_ignore_ascii_case("n/d") {
        return None;
    }
    let price = Money::try_parse("stooq_close", close).ok()?;
    Some(PriceQuote {
        price,
        provenance: PriceProvenance::Market {
            source: STOOQ_SOURCE.to_owned(),
        },
        as_of: chrono::Utc::now(),
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn ticker(s: &str) -> Ticker {
        Ticker::try_new(s).unwrap()
    }

    #[test]
    fn builds_the_lowercased_dot_us_symbol() {
        assert_eq!(StooqMarketData::stooq_symbol(&ticker("AAPL")), "aapl.us");
        assert_eq!(StooqMarketData::stooq_symbol(&ticker("brk.a")), "brk.a.us");
    }

    #[test]
    fn parses_close_from_a_captured_csv() {
        let csv = "Symbol,Date,Time,Open,High,Low,Close,Volume\n\
                   AAPL.US,2026-06-11,22:00:01,180.0,182.0,179.5,180.25,1234567\n";
        let quote = parse_stooq_csv(csv).expect("a well-formed row resolves");
        assert_eq!(quote.price, Money::try_parse("x", "180.25").unwrap());
        assert_eq!(
            quote.provenance,
            PriceProvenance::Market {
                source: "stooq".to_owned()
            }
        );
    }

    #[test]
    fn no_data_close_degrades_to_none() {
        let csv = "Symbol,Date,Time,Open,High,Low,Close,Volume\n\
                   ZZZZ.US,N/D,N/D,N/D,N/D,N/D,N/D,N/D\n";
        assert_eq!(parse_stooq_csv(csv), None);
    }

    #[test]
    fn header_only_response_degrades_to_none() {
        let csv = "Symbol,Date,Time,Open,High,Low,Close,Volume\n";
        assert_eq!(parse_stooq_csv(csv), None);
    }

    #[test]
    fn short_row_degrades_to_none() {
        let csv = "Symbol,Date,Time\nAAPL.US,2026-06-11,22:00:01\n";
        assert_eq!(parse_stooq_csv(csv), None);
    }
}
