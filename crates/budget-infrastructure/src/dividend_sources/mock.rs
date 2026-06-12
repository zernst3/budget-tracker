//! [`MockDividendSource`] — a fixture-configured dividend adapter
//! (`docs/DRIP_REALTIME_DESIGN.md §5`, mirrors [`MockMarketDataProvider`]).
//!
//! Configured with a per-ticker response map; `dividends_since` replays whatever
//! was registered for the ticker (a `Vec<DividendEvent>` or an error), filtered to
//! `pay_date > since`, defaulting to `Ok(vec![])` for unregistered tickers. No
//! network. This is the source every mock-only catch-up test grounds against.
//!
//! [`MockMarketDataProvider`]: crate::market_data::MockMarketDataProvider

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use chrono::NaiveDate;

use budget_domain::portfolio::{DividendEvent, DividendSource, DividendSourceError, Ticker};

/// A canned per-ticker response for [`MockDividendSource`]: either a list of
/// events, or a deliberate error.
#[derive(Debug, Clone)]
pub enum MockDividends {
    /// The full set of known dividend events for the ticker (the source filters
    /// them by `pay_date > since` on each call).
    Events(Vec<DividendEvent>),
    /// A source error for the ticker (drives the error path).
    Error(DividendSourceError),
}

/// A fixture-configured [`DividendSource`] (no network).
///
/// Unregistered tickers default to `Ok(vec![])` so a catch-up over a ticker the
/// fixture forgot degrades cleanly (no accretion) rather than panicking.
#[derive(Debug, Default)]
pub struct MockDividendSource {
    responses: Mutex<HashMap<String, MockDividends>>,
}

impl MockDividendSource {
    /// An empty source — every ticker resolves to `Ok(vec![])`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            responses: Mutex::new(HashMap::new()),
        }
    }

    /// Register a canned response for `ticker` (chainable builder).
    #[must_use]
    pub fn with(self, ticker: &Ticker, response: MockDividends) -> Self {
        if let Ok(mut map) = self.responses.lock() {
            map.insert(ticker.as_str().to_owned(), response);
        }
        self
    }

    /// Register a list of dividend events for `ticker` (chainable convenience).
    #[must_use]
    pub fn with_events(self, ticker: &Ticker, events: Vec<DividendEvent>) -> Self {
        self.with(ticker, MockDividends::Events(events))
    }
}

#[async_trait]
impl DividendSource for MockDividendSource {
    async fn dividends_since(
        &self,
        ticker: &Ticker,
        since: NaiveDate,
    ) -> Result<Vec<DividendEvent>, DividendSourceError> {
        let registered = self
            .responses
            .lock()
            .ok()
            .and_then(|map| map.get(ticker.as_str()).cloned());
        match registered {
            None => Ok(vec![]),
            Some(MockDividends::Events(events)) => {
                Ok(events.into_iter().filter(|e| e.pay_date > since).collect())
            }
            Some(MockDividends::Error(e)) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use budget_domain::money::Money;
    use budget_domain::portfolio::DividendSourceKind;

    fn ticker(s: &str) -> Ticker {
        Ticker::try_new(s).unwrap()
    }

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    fn event(ticker_str: &str, pay: NaiveDate, cents: i64) -> DividendEvent {
        DividendEvent {
            ticker: ticker(ticker_str),
            ex_date: pay - chrono::Duration::days(7),
            pay_date: pay,
            amount_per_share: Money::from_minor(cents),
            source: DividendSourceKind::Mock,
        }
    }

    #[tokio::test]
    async fn returns_configured_events_filtered_by_since() {
        let source = MockDividendSource::new().with_events(
            &ticker("AAPL"),
            vec![
                event("AAPL", date(2026, 2, 15), 24),
                event("AAPL", date(2026, 5, 15), 25),
            ],
        );
        // since = 2026-03-01 suppresses the February dividend.
        let out = source
            .dividends_since(&ticker("AAPL"), date(2026, 3, 1))
            .await
            .unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].pay_date, date(2026, 5, 15));
    }

    #[tokio::test]
    async fn unregistered_ticker_returns_empty() {
        let source = MockDividendSource::new();
        let out = source
            .dividends_since(&ticker("ZZZZ"), date(2026, 1, 1))
            .await
            .unwrap();
        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn configured_error_propagates() {
        let source = MockDividendSource::new().with(
            &ticker("NVDA"),
            MockDividends::Error(DividendSourceError::RateLimited("slow down".to_owned())),
        );
        let out = source
            .dividends_since(&ticker("NVDA"), date(2026, 1, 1))
            .await;
        assert_eq!(
            out,
            Err(DividendSourceError::RateLimited("slow down".to_owned()))
        );
    }
}
