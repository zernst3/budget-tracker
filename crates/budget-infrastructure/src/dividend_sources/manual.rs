//! [`ManualDividendSource`] — a static user-confirmed dividend tier
//! (`docs/DRIP_REALTIME_DESIGN.md §7`, the ultimate fallback; mirrors
//! [`ManualPriceSource`]).
//!
//! Seeded at wiring time with a ticker→`Vec<DividendEvent>` map: the rare case
//! where neither Tiingo nor Yahoo has a dividend and the user confirms a
//! `$/share` by hand. Empty in v1 (the chain is then Tiingo → Yahoo → none).
//! Events are tagged [`DividendSourceKind::Manual`] regardless of how they were
//! registered, so the cache records the manual provenance.
//!
//! [`ManualPriceSource`]: crate::market_data::ManualPriceSource

use std::collections::HashMap;

use async_trait::async_trait;
use chrono::NaiveDate;

use budget_domain::portfolio::{
    DividendEvent, DividendSource, DividendSourceError, DividendSourceKind, Ticker,
};

/// A static ticker→events manual dividend tier ([`DividendSourceKind::Manual`]).
///
/// Seeded at wiring time. Empty in v1.
#[derive(Debug, Default)]
pub struct ManualDividendSource {
    events: HashMap<String, Vec<DividendEvent>>,
}

impl ManualDividendSource {
    /// An empty manual source — every ticker resolves to `Ok(vec![])`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            events: HashMap::new(),
        }
    }

    /// Register a manual dividend for `ticker` (chainable builder). The stored
    /// event's `source` is forced to [`DividendSourceKind::Manual`].
    #[must_use]
    pub fn with(mut self, ticker: &Ticker, mut event: DividendEvent) -> Self {
        event.source = DividendSourceKind::Manual;
        self.events
            .entry(ticker.as_str().to_owned())
            .or_default()
            .push(event);
        self
    }
}

#[async_trait]
impl DividendSource for ManualDividendSource {
    async fn dividends_since(
        &self,
        ticker: &Ticker,
        since: NaiveDate,
    ) -> Result<Vec<DividendEvent>, DividendSourceError> {
        Ok(self
            .events
            .get(ticker.as_str())
            .into_iter()
            .flatten()
            .filter(|e| e.pay_date > since)
            .cloned()
            .collect())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use budget_domain::money::Money;

    fn ticker(s: &str) -> Ticker {
        Ticker::try_new(s).unwrap()
    }

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    fn event(pay: NaiveDate, cents: i64, source: DividendSourceKind) -> DividendEvent {
        DividendEvent {
            ticker: ticker("KO"),
            ex_date: pay - chrono::Duration::days(7),
            pay_date: pay,
            amount_per_share: Money::from_minor(cents),
            source,
        }
    }

    #[tokio::test]
    async fn empty_source_is_empty() {
        let source = ManualDividendSource::new();
        assert!(
            source
                .dividends_since(&ticker("KO"), date(2026, 1, 1))
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn registered_event_is_tagged_manual_and_filtered() {
        // Register with a non-manual source; the tier forces Manual provenance.
        let source = ManualDividendSource::new().with(
            &ticker("KO"),
            event(date(2026, 4, 1), 46, DividendSourceKind::Tiingo),
        );
        let out = source
            .dividends_since(&ticker("KO"), date(2026, 1, 1))
            .await
            .unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].source, DividendSourceKind::Manual);
        assert_eq!(out[0].amount_per_share, Money::from_minor(46));

        // since after the pay-date suppresses it.
        let none = source
            .dividends_since(&ticker("KO"), date(2026, 6, 1))
            .await
            .unwrap();
        assert!(none.is_empty());
    }
}
