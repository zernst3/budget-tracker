//! [`YahooDividendSource`] — the keyless dividend fallback tier
//! (`docs/DRIP_REALTIME_DESIGN.md §7`).
//!
//! Yahoo's keyless v8 chart endpoint
//! `GET /v8/finance/chart/<ticker>?period1=<epoch>&period2=<epoch>&interval=1d&events=div`
//! returns a JSON envelope whose `chart.result[0].events.dividends` is a map
//! keyed by epoch-seconds → `{ "amount": f64, "date": i64 }`. Because it needs no
//! API key, it is the tier that lets the dividend feature run with NO configured
//! key (Tiingo upgrades to the primary source when a key is present).
//!
//! ## Date semantics (`ORCH-TRAINING-CUTOFF-1`: best-effort, confirm at smoke time)
//!
//! Yahoo's `date` is the ex-dividend date (epoch-seconds). Like Tiingo, the free
//! envelope carries no separate pay-date, so this adapter uses that date as BOTH
//! `ex_date` and `pay_date` (best-estimate, self-correcting on re-upload per §2.1).
//!
//! ## Provenance
//!
//! A resolved event is tagged [`DividendSourceKind::Yahoo`].

use async_trait::async_trait;
use chrono::{DateTime, NaiveDate};
use serde::Deserialize;
use std::collections::HashMap;

use budget_domain::money::Money;
use budget_domain::portfolio::{
    DividendEvent, DividendSource, DividendSourceError, DividendSourceKind, Ticker,
};

/// The Yahoo v8 chart base URL.
const YAHOO_BASE: &str = "https://query1.finance.yahoo.com/v8/finance/chart";

/// The Yahoo v8 chart envelope (only the fields we consume).
#[derive(Debug, Clone, Deserialize)]
struct YahooEnvelope {
    chart: YahooChart,
}

#[derive(Debug, Clone, Deserialize)]
struct YahooChart {
    #[serde(default)]
    result: Vec<YahooResult>,
}

#[derive(Debug, Clone, Deserialize)]
struct YahooResult {
    #[serde(default)]
    events: YahooEvents,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct YahooEvents {
    /// epoch-seconds key → dividend entry.
    #[serde(default)]
    dividends: HashMap<String, YahooDividend>,
}

#[derive(Debug, Clone, Deserialize)]
struct YahooDividend {
    #[serde(default)]
    amount: f64,
    /// Ex-dividend date, epoch-seconds.
    #[serde(default)]
    date: i64,
}

/// The keyless Yahoo dividend [`DividendSource`].
pub struct YahooDividendSource {
    http: reqwest::Client,
    base_url: String,
}

impl Default for YahooDividendSource {
    fn default() -> Self {
        Self::new()
    }
}

impl YahooDividendSource {
    /// Build the source against the live Yahoo endpoint.
    #[must_use]
    pub fn new() -> Self {
        Self::with_base_url(YAHOO_BASE.to_owned())
    }

    /// Build against an explicit base URL (tests point this at a local fixture
    /// server; production uses [`YahooDividendSource::new`]).
    #[must_use]
    pub fn with_base_url(base_url: String) -> Self {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .unwrap_or_default();
        Self { http, base_url }
    }
}

#[async_trait]
impl DividendSource for YahooDividendSource {
    async fn dividends_since(
        &self,
        ticker: &Ticker,
        since: NaiveDate,
    ) -> Result<Vec<DividendEvent>, DividendSourceError> {
        // period1 = day after `since` (cutoff exclusive); period2 = far future.
        let period1 = since
            .succ_opt()
            .unwrap_or(since)
            .and_hms_opt(0, 0, 0)
            .map_or(0, |dt| dt.and_utc().timestamp());
        let period2 = period1 + 60 * 60 * 24 * 366 * 10; // ~10y window.
        let url = format!(
            "{base}/{symbol}?period1={period1}&period2={period2}&interval=1d&events=div",
            base = self.base_url,
            symbol = ticker.as_str(),
        );
        let response = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| DividendSourceError::Api(format!("yahoo request failed: {e}")))?;
        let status = response.status();
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(DividendSourceError::RateLimited(format!(
                "yahoo http {status}"
            )));
        }
        if !status.is_success() {
            return Err(DividendSourceError::Api(format!("yahoo http {status}")));
        }
        let body = response
            .text()
            .await
            .map_err(|e| DividendSourceError::Api(format!("yahoo body read failed: {e}")))?;

        parse_yahoo_dividends(ticker, since, &body)
    }
}

/// Parse a Yahoo v8 chart body into the dividend events with `pay_date > since`
/// (Yahoo's `date` is used as both ex- and pay-date, §7).
///
/// Pure (no I/O) so it is unit-tested against captured payloads. Entries with a
/// non-positive amount or an unrepresentable epoch are skipped. A body that is
/// not valid Yahoo JSON is a typed [`DividendSourceError::Api`].
///
/// # Errors
/// [`DividendSourceError::Api`] if the body is not valid Yahoo chart JSON.
fn parse_yahoo_dividends(
    ticker: &Ticker,
    since: NaiveDate,
    body: &str,
) -> Result<Vec<DividendEvent>, DividendSourceError> {
    let envelope: YahooEnvelope = serde_json::from_str(body)
        .map_err(|e| DividendSourceError::Api(format!("yahoo decode failed: {e}")))?;
    let Some(result) = envelope.chart.result.into_iter().next() else {
        return Ok(vec![]);
    };
    let mut events = Vec::new();
    for entry in result.events.dividends.into_values() {
        if entry.amount <= 0.0 {
            continue;
        }
        let Some(date) = DateTime::from_timestamp(entry.date, 0).map(|dt| dt.date_naive()) else {
            continue;
        };
        if date <= since {
            continue;
        }
        // f64 wire unit → exact Money via its string form (BUDGET-MONEY-1).
        let Ok(amount) = Money::try_parse("yahoo_div", &entry.amount.to_string()) else {
            continue;
        };
        events.push(DividendEvent {
            ticker: ticker.clone(),
            ex_date: date,
            pay_date: date,
            amount_per_share: amount,
            source: DividendSourceKind::Yahoo,
        });
    }
    // The dividends map is unordered; sort chronological by pay-date.
    events.sort_by_key(|e| e.pay_date);
    Ok(events)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn ticker(s: &str) -> Ticker {
        Ticker::try_new(s).unwrap()
    }

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    // A captured-shape Yahoo v8 chart payload with two dividends.
    // 1770681600 = 2026-02-10T00:00:00Z, 1778803200 = 2026-05-15T00:00:00Z.
    const SAMPLE: &str = r#"{
        "chart": {
            "result": [
                {
                    "meta": {"symbol": "AAPL"},
                    "events": {
                        "dividends": {
                            "1770681600": {"amount": 0.24, "date": 1770681600},
                            "1778803200": {"amount": 0.25, "date": 1778803200}
                        }
                    }
                }
            ],
            "error": null
        }
    }"#;

    #[test]
    fn parses_dividends_sorted_chronologically() {
        let out = parse_yahoo_dividends(&ticker("AAPL"), date(2026, 1, 1), SAMPLE).unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].pay_date, date(2026, 2, 10));
        assert_eq!(out[1].pay_date, date(2026, 5, 15));
        assert_eq!(
            out[1].amount_per_share,
            Money::try_parse("x", "0.25").unwrap()
        );
        assert_eq!(out[0].source, DividendSourceKind::Yahoo);
    }

    #[test]
    fn suppresses_dividends_on_or_before_since() {
        let out = parse_yahoo_dividends(&ticker("AAPL"), date(2026, 2, 10), SAMPLE).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].pay_date, date(2026, 5, 15));
    }

    #[test]
    fn missing_result_is_no_dividends() {
        let body = r#"{"chart":{"result":[],"error":null}}"#;
        let out = parse_yahoo_dividends(&ticker("AAPL"), date(2026, 1, 1), body).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn garbage_body_is_a_typed_api_error() {
        let out = parse_yahoo_dividends(&ticker("AAPL"), date(2026, 1, 1), "not json");
        assert!(matches!(out, Err(DividendSourceError::Api(_))));
    }
}
