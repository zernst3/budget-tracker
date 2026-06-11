//! [`ChainMarketDataProvider`] — the fallback chain
//! (`docs/AI_FEATURE_DESIGN.md §Phase 6`, Zach's resolved decision #2):
//! **Finnhub (real, key from vault) → Stooq (keyless CSV) → manual price →
//! degrade to `None`.**
//!
//! The chain IS the resilience layer, so — unlike the snapshot-assembly fan-out,
//! which propagates a provider `Err` — a per-tier error here is logged (WARN) and
//! the chain falls through to the next tier. Only when EVERY tier yields nothing
//! does it return `Ok(None)` (which `assemble_snapshot` turns into a degraded
//! `quote: None` position → any citing claim reconciles to
//! `MissingMarketData`). A misconfigured prod thus still produces a review (with
//! degraded positions), rather than a hard failure.
//!
//! ## Why the chain means "runs with NO API key"
//!
//! The Finnhub tier reads its key from the vault per call; if the key is absent
//! it errors, the chain logs + falls through to Stooq (keyless) and then manual.
//! So the real feature runs end-to-end with no configured key; the Finnhub key
//! only UPGRADES to real-time quotes.
//!
//! ## The manual tier
//!
//! The locked [`Position`] shape carries no per-position manual *price* field, so
//! the manual tier is a configurable ticker→price map ([`ManualPriceSource`])
//! seeded at wiring time (empty in v1 → the chain is effectively Finnhub → Stooq
//! → None). A manual hit is tagged [`PriceProvenance::Manual`]. When a per-position
//! manual-price field is added later it feeds this same tier; the chain shape does
//! not change.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;

use budget_domain::money::Money;
use budget_domain::portfolio::{
    MarketDataError, MarketDataProvider, PriceProvenance, PriceQuote, Ticker,
};

/// A static ticker→price manual fallback tier ([`PriceProvenance::Manual`]).
///
/// Seeded at wiring time. Empty in v1 (the chain is then Finnhub → Stooq → None).
#[derive(Debug, Default)]
pub struct ManualPriceSource {
    prices: HashMap<String, Money>,
}

impl ManualPriceSource {
    /// An empty manual source — every ticker resolves to `Ok(None)`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            prices: HashMap::new(),
        }
    }

    /// Register a manual price for `ticker` (chainable builder).
    #[must_use]
    pub fn with(mut self, ticker: &Ticker, price: Money) -> Self {
        self.prices.insert(ticker.as_str().to_owned(), price);
        self
    }
}

#[async_trait]
impl MarketDataProvider for ManualPriceSource {
    async fn quote(&self, ticker: &Ticker) -> Result<Option<PriceQuote>, MarketDataError> {
        Ok(self.prices.get(ticker.as_str()).map(|price| PriceQuote {
            price: *price,
            provenance: PriceProvenance::Manual,
            as_of: chrono::Utc::now(),
        }))
    }
}

/// An ordered fallback chain over several [`MarketDataProvider`] tiers.
///
/// `quote` tries each tier in order: a tier's `Ok(Some(quote))` wins; an
/// `Ok(None)` or an `Err` (logged WARN) falls through to the next tier. After the
/// last tier, the chain degrades to `Ok(None)`.
pub struct ChainMarketDataProvider {
    tiers: Vec<Arc<dyn MarketDataProvider>>,
}

impl ChainMarketDataProvider {
    /// Build the chain from an ordered tier list (first = highest priority).
    #[must_use]
    pub fn new(tiers: Vec<Arc<dyn MarketDataProvider>>) -> Self {
        Self { tiers }
    }
}

#[async_trait]
impl MarketDataProvider for ChainMarketDataProvider {
    async fn quote(&self, ticker: &Ticker) -> Result<Option<PriceQuote>, MarketDataError> {
        for (index, tier) in self.tiers.iter().enumerate() {
            match tier.quote(ticker).await {
                Ok(Some(quote)) => return Ok(Some(quote)),
                Ok(None) => {} // no quote at this tier; fall through.
                Err(err) => {
                    // The chain is the resilience layer: a per-tier error is not
                    // fatal — log it (no secret material is in a MarketDataError,
                    // §0.3) and fall through to the next tier.
                    tracing::warn!(
                        ticker = ticker.as_str(),
                        tier = index,
                        error = %err,
                        "market-data tier failed; falling through to the next tier",
                    );
                }
            }
        }
        // Every tier yielded nothing: degrade. assemble_snapshot turns this into a
        // `quote: None` position, and a citing claim reconciles to
        // MissingMarketData.
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::market_data::{MockMarketDataProvider, MockQuote};
    use chrono::Utc;

    fn ticker(s: &str) -> Ticker {
        Ticker::try_new(s).unwrap()
    }

    fn quote(source: &str, cents: i64) -> PriceQuote {
        PriceQuote {
            price: Money::from_minor(cents),
            provenance: PriceProvenance::Market {
                source: source.to_owned(),
            },
            as_of: Utc::now(),
        }
    }

    #[tokio::test]
    async fn first_tier_hit_wins() {
        let t1 = Arc::new(
            MockMarketDataProvider::new().with_quote(&ticker("AAPL"), quote("finnhub", 18_000)),
        );
        let t2 = Arc::new(
            MockMarketDataProvider::new().with_quote(&ticker("AAPL"), quote("stooq", 99_999)),
        );
        let chain = ChainMarketDataProvider::new(vec![t1, t2]);
        let out = chain.quote(&ticker("AAPL")).await.unwrap().unwrap();
        assert_eq!(out.price, Money::from_minor(18_000));
        assert_eq!(
            out.provenance,
            PriceProvenance::Market {
                source: "finnhub".to_owned()
            }
        );
    }

    #[tokio::test]
    async fn falls_through_on_none_to_the_next_tier() {
        // Tier 1 has no quote (Ok(None)); tier 2 (stooq) resolves.
        let t1 = Arc::new(MockMarketDataProvider::new()); // empty -> Ok(None)
        let t2 = Arc::new(
            MockMarketDataProvider::new().with_quote(&ticker("AAPL"), quote("stooq", 18_025)),
        );
        let chain = ChainMarketDataProvider::new(vec![t1, t2]);
        let out = chain.quote(&ticker("AAPL")).await.unwrap().unwrap();
        assert_eq!(
            out.provenance,
            PriceProvenance::Market {
                source: "stooq".to_owned()
            }
        );
    }

    #[tokio::test]
    async fn falls_through_on_error_to_the_next_tier() {
        // Tier 1 errors (e.g. no Finnhub key / rate limit); the chain swallows it
        // and tier 2 resolves. This is the "runs with NO API key" guarantee.
        let t1 = Arc::new(MockMarketDataProvider::new().with(
            &ticker("AAPL"),
            MockQuote::Error(MarketDataError::SecretVault("no finnhub key".to_owned())),
        ));
        let t2 = Arc::new(
            MockMarketDataProvider::new().with_quote(&ticker("AAPL"), quote("stooq", 18_050)),
        );
        let chain = ChainMarketDataProvider::new(vec![t1, t2]);
        let out = chain.quote(&ticker("AAPL")).await.unwrap().unwrap();
        assert_eq!(out.price, Money::from_minor(18_050));
    }

    #[tokio::test]
    async fn manual_tier_resolves_when_feeds_miss() {
        // Finnhub + Stooq both miss; the manual tier resolves with Manual provenance.
        let finnhub = Arc::new(MockMarketDataProvider::new()); // Ok(None)
        let stooq = Arc::new(MockMarketDataProvider::new()); // Ok(None)
        let manual =
            Arc::new(ManualPriceSource::new().with(&ticker("AAPL"), Money::from_minor(17_500)));
        let chain = ChainMarketDataProvider::new(vec![finnhub, stooq, manual]);
        let out = chain.quote(&ticker("AAPL")).await.unwrap().unwrap();
        assert_eq!(out.price, Money::from_minor(17_500));
        assert_eq!(out.provenance, PriceProvenance::Manual);
    }

    #[tokio::test]
    async fn all_tiers_miss_degrades_to_none() {
        let chain = ChainMarketDataProvider::new(vec![
            Arc::new(MockMarketDataProvider::new()),
            Arc::new(ManualPriceSource::new()),
        ]);
        assert_eq!(chain.quote(&ticker("ZZZZ")).await.unwrap(), None);
    }

    #[tokio::test]
    async fn empty_manual_source_is_none() {
        let manual = ManualPriceSource::new();
        assert_eq!(manual.quote(&ticker("AAPL")).await.unwrap(), None);
    }
}
