//! [`MockMarketDataProvider`] — a fixture-configured market-data adapter
//! (`docs/AI_FEATURE_DESIGN.md §Phase 3`, mirrors `MockPlaidApi`).
//!
//! Configured with a per-ticker response map; `quote` replays whatever was
//! registered for the ticker (a quote, `Ok(None)`, or an error), defaulting to
//! `Ok(None)` for unregistered tickers (so the caller degrades / falls back).
//! No network. This is the provider every mock-only test below the UI grounds
//! against, and (Phase 6) the provider `AI_MODE=mock` selects.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;

use budget_domain::portfolio::{MarketDataError, MarketDataProvider, PriceQuote, Ticker};

/// A canned per-ticker response for [`MockMarketDataProvider`]: either a quote, a
/// deliberate "no quote" (`None`), or a deliberate error.
#[derive(Debug, Clone)]
pub enum MockQuote {
    /// A resolved quote for the ticker.
    Quote(PriceQuote),
    /// No quote available (the caller falls back to a manual price or degrades).
    NoQuote,
    /// A provider error for the ticker (drives the degraded path).
    Error(MarketDataError),
}

/// A fixture-configured [`MarketDataProvider`] (no network).
///
/// Unregistered tickers default to `Ok(None)` so a snapshot assembly with a
/// ticker the fixture forgot degrades cleanly rather than panicking.
#[derive(Debug, Default)]
pub struct MockMarketDataProvider {
    responses: Mutex<HashMap<String, MockQuote>>,
}

impl MockMarketDataProvider {
    /// An empty provider — every ticker resolves to `Ok(None)`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            responses: Mutex::new(HashMap::new()),
        }
    }

    /// Register a canned response for `ticker` (chainable builder).
    #[must_use]
    pub fn with(self, ticker: &Ticker, response: MockQuote) -> Self {
        if let Ok(mut map) = self.responses.lock() {
            map.insert(ticker.as_str().to_owned(), response);
        }
        self
    }

    /// Register a resolved quote for `ticker` (chainable convenience).
    #[must_use]
    pub fn with_quote(self, ticker: &Ticker, quote: PriceQuote) -> Self {
        self.with(ticker, MockQuote::Quote(quote))
    }
}

#[async_trait]
impl MarketDataProvider for MockMarketDataProvider {
    async fn quote(&self, ticker: &Ticker) -> Result<Option<PriceQuote>, MarketDataError> {
        let registered = self
            .responses
            .lock()
            .ok()
            .and_then(|map| map.get(ticker.as_str()).cloned());
        match registered {
            None | Some(MockQuote::NoQuote) => Ok(None),
            Some(MockQuote::Quote(q)) => Ok(Some(q)),
            Some(MockQuote::Error(e)) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use budget_domain::money::Money;
    use budget_domain::portfolio::PriceProvenance;
    use chrono::Utc;

    fn ticker(s: &str) -> Ticker {
        Ticker::try_new(s).unwrap()
    }

    fn sample_quote() -> PriceQuote {
        PriceQuote {
            price: Money::from_minor(18_000),
            provenance: PriceProvenance::Market {
                source: "mock".to_owned(),
            },
            as_of: Utc::now(),
        }
    }

    #[tokio::test]
    async fn returns_configured_quote() {
        let provider = MockMarketDataProvider::new().with_quote(&ticker("AAPL"), sample_quote());
        let out = provider.quote(&ticker("AAPL")).await.unwrap();
        assert_eq!(out.map(|q| q.price), Some(Money::from_minor(18_000)));
    }

    #[tokio::test]
    async fn unregistered_ticker_returns_none() {
        let provider = MockMarketDataProvider::new();
        let out = provider.quote(&ticker("ZZZZ")).await.unwrap();
        assert_eq!(out, None);
    }

    #[tokio::test]
    async fn explicit_no_quote_returns_none() {
        let provider = MockMarketDataProvider::new().with(&ticker("TSLA"), MockQuote::NoQuote);
        assert_eq!(provider.quote(&ticker("TSLA")).await.unwrap(), None);
    }

    #[tokio::test]
    async fn configured_error_propagates() {
        let provider = MockMarketDataProvider::new().with(
            &ticker("NVDA"),
            MockQuote::Error(MarketDataError::RateLimited("slow down".to_owned())),
        );
        let out = provider.quote(&ticker("NVDA")).await;
        assert_eq!(
            out,
            Err(MarketDataError::RateLimited("slow down".to_owned()))
        );
    }
}
