//! [`ChainDividendSource`] — the dividend fallback chain
//! (`docs/DRIP_REALTIME_DESIGN.md §6/§7`): **Tiingo → Yahoo → manual.**
//!
//! The same resilience shape as
//! [`ChainMarketDataProvider`](crate::market_data::ChainMarketDataProvider): the
//! chain IS the resilience layer, so a per-tier error is logged (WARN) and the
//! chain falls through to the next tier. The FIRST tier that returns a non-empty
//! event list wins; an empty list or an error falls through. After the last tier,
//! the chain returns `Ok(vec![])` (no dividends — no accretion, which is the safe
//! conservative outcome).
//!
//! ## Why "first non-empty wins" (not merge)
//!
//! A dividend is one real-world fact; the tiers are alternative SOURCES of the
//! same fact, not complementary slices. Merging would risk double-counting a
//! dividend reported by two tiers with slightly different dates. Taking the
//! highest-priority tier that has data keeps the per-`(position, pay_date)`
//! idempotency guard meaningful and the estimate conservative.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::NaiveDate;

use budget_domain::portfolio::{DividendEvent, DividendSource, DividendSourceError, Ticker};

/// An ordered fallback chain over several [`DividendSource`] tiers.
///
/// `dividends_since` tries each tier in order: the first tier whose result is a
/// non-empty `Ok(events)` wins; an `Ok(vec![])` or an `Err` (logged WARN) falls
/// through. After the last tier, the chain returns `Ok(vec![])`.
pub struct ChainDividendSource {
    tiers: Vec<Arc<dyn DividendSource>>,
}

impl ChainDividendSource {
    /// Build the chain from an ordered tier list (first = highest priority).
    #[must_use]
    pub fn new(tiers: Vec<Arc<dyn DividendSource>>) -> Self {
        Self { tiers }
    }
}

#[async_trait]
impl DividendSource for ChainDividendSource {
    async fn dividends_since(
        &self,
        ticker: &Ticker,
        since: NaiveDate,
    ) -> Result<Vec<DividendEvent>, DividendSourceError> {
        for (index, tier) in self.tiers.iter().enumerate() {
            match tier.dividends_since(ticker, since).await {
                Ok(events) if !events.is_empty() => return Ok(events),
                Ok(_) => {} // empty at this tier; fall through.
                Err(err) => {
                    // A per-tier error is not fatal (no secret material is in a
                    // DividendSourceError, §0.3): log it and fall through.
                    tracing::warn!(
                        ticker = ticker.as_str(),
                        tier = index,
                        error = %err,
                        "dividend-source tier failed; falling through to the next tier",
                    );
                }
            }
        }
        // Every tier yielded nothing: no dividends → no accretion (conservative).
        Ok(vec![])
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::dividend_sources::manual::ManualDividendSource;
    use crate::dividend_sources::mock::{MockDividendSource, MockDividends};
    use budget_domain::money::Money;
    use budget_domain::portfolio::DividendSourceKind;

    fn ticker(s: &str) -> Ticker {
        Ticker::try_new(s).unwrap()
    }

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    fn event(source: DividendSourceKind, cents: i64) -> DividendEvent {
        let pay = date(2026, 5, 15);
        DividendEvent {
            ticker: ticker("AAPL"),
            ex_date: pay - chrono::Duration::days(7),
            pay_date: pay,
            amount_per_share: Money::from_minor(cents),
            source,
        }
    }

    #[tokio::test]
    async fn first_non_empty_tier_wins() {
        let tiingo = Arc::new(
            MockDividendSource::new()
                .with_events(&ticker("AAPL"), vec![event(DividendSourceKind::Tiingo, 25)]),
        );
        let yahoo = Arc::new(
            MockDividendSource::new()
                .with_events(&ticker("AAPL"), vec![event(DividendSourceKind::Yahoo, 99)]),
        );
        let chain = ChainDividendSource::new(vec![tiingo, yahoo]);
        let out = chain
            .dividends_since(&ticker("AAPL"), date(2026, 1, 1))
            .await
            .unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].source, DividendSourceKind::Tiingo);
        assert_eq!(out[0].amount_per_share, Money::from_minor(25));
    }

    #[tokio::test]
    async fn falls_through_empty_then_error_to_manual() {
        // Tier 1 (Tiingo) empty; tier 2 (Yahoo) errors; tier 3 (manual) resolves.
        let tiingo = Arc::new(MockDividendSource::new()); // empty
        let yahoo = Arc::new(MockDividendSource::new().with(
            &ticker("AAPL"),
            MockDividends::Error(DividendSourceError::Api("yahoo down".to_owned())),
        ));
        let manual = Arc::new(
            ManualDividendSource::new()
                .with(&ticker("AAPL"), event(DividendSourceKind::Manual, 46)),
        );
        let chain = ChainDividendSource::new(vec![tiingo, yahoo, manual]);
        let out = chain
            .dividends_since(&ticker("AAPL"), date(2026, 1, 1))
            .await
            .unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].source, DividendSourceKind::Manual);
    }

    #[tokio::test]
    async fn all_tiers_empty_yields_no_dividends() {
        let chain = ChainDividendSource::new(vec![
            Arc::new(MockDividendSource::new()),
            Arc::new(ManualDividendSource::new()),
        ]);
        let out = chain
            .dividends_since(&ticker("ZZZZ"), date(2026, 1, 1))
            .await
            .unwrap();
        assert!(out.is_empty());
    }
}
