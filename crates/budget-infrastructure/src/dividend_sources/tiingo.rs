//! [`TiingoDividendSource`] — the primary free dividend tier
//! (`docs/DRIP_REALTIME_DESIGN.md §7`).
//!
//! Tiingo's daily-prices endpoint
//! `GET /tiingo/daily/<ticker>/prices?startDate=<d>&columns=date,divCash&format=json`
//! returns one JSON object per trading day; the `divCash` field is the cash
//! dividend per share, non-zero only on a dividend day. The API key is read from
//! the [`SecretVault`] (`BUDGET-PLAID-TOKEN-VAULT-1`: never config/env/logs) and
//! sent as `Authorization: Token <key>`.
//!
//! ## Date semantics (`ORCH-TRAINING-CUTOFF-1`: best-effort, confirm at smoke time)
//!
//! Tiingo's free daily endpoint reports the dividend cash on the **ex-date** row
//! and does NOT carry a separate pay-date. So this adapter uses that `date` as
//! BOTH `ex_date` and `pay_date` (a conservative best-estimate — the design's §2.1
//! "deltas reconciled on each re-upload" makes a small pay-date offset
//! self-correcting). When a confirmed pay-date source is wired later it slots in
//! here without changing the port.
//!
//! ## Provenance
//!
//! A resolved event is tagged [`DividendSourceKind::Tiingo`].

use std::sync::Arc;

use async_trait::async_trait;
use chrono::NaiveDate;
use serde::Deserialize;

use budget_domain::auth::SecretVault;
use budget_domain::money::Money;
use budget_domain::portfolio::{
    DividendEvent, DividendSource, DividendSourceError, DividendSourceKind, Ticker,
};

/// The vault secret name the Tiingo API key is read under
/// (`BUDGET-PLAID-TOKEN-VAULT-1`).
pub const TIINGO_API_KEY_SECRET: &str = "tiingo-api-key";

/// The Tiingo daily-prices base URL.
const TIINGO_BASE: &str = "https://api.tiingo.com/tiingo/daily";

/// One Tiingo daily-prices row (only the fields we consume).
#[derive(Debug, Clone, Deserialize)]
struct TiingoDailyRow {
    /// The trading day (ISO-8601, possibly with a time component).
    date: String,
    /// Cash dividend per share that day (`0.0` on non-dividend days).
    #[serde(default, rename = "divCash")]
    div_cash: f64,
}

/// The Tiingo free-tier dividend [`DividendSource`].
pub struct TiingoDividendSource {
    vault: Arc<dyn SecretVault>,
    http: reqwest::Client,
    base_url: String,
}

impl TiingoDividendSource {
    /// Build the source against the live Tiingo endpoint, reading the API key
    /// from `vault` per call.
    #[must_use]
    pub fn new(vault: Arc<dyn SecretVault>) -> Self {
        Self::with_base_url(vault, TIINGO_BASE.to_owned())
    }

    /// Build against an explicit base URL (tests point this at a local fixture
    /// server; production uses [`TiingoDividendSource::new`]).
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
impl DividendSource for TiingoDividendSource {
    async fn dividends_since(
        &self,
        ticker: &Ticker,
        since: NaiveDate,
    ) -> Result<Vec<DividendEvent>, DividendSourceError> {
        let api_key = self
            .vault
            .get_secret(TIINGO_API_KEY_SECRET)
            .await
            .map_err(|e| DividendSourceError::SecretVault(e.to_string()))?;

        // Fetch from the day AFTER `since` (the cutoff is exclusive); Tiingo's
        // startDate is inclusive, so request since+1.
        let start = since
            .succ_opt()
            .unwrap_or(since)
            .format("%Y-%m-%d")
            .to_string();
        let url = format!(
            "{base}/{symbol}/prices?startDate={start}&columns=date,divCash&format=json",
            base = self.base_url,
            symbol = ticker.as_str().to_lowercase(),
        );
        let response = self
            .http
            .get(&url)
            .header("Authorization", format!("Token {}", api_key.as_str()))
            .send()
            .await
            .map_err(|e| DividendSourceError::Api(format!("tiingo request failed: {e}")))?;
        let status = response.status();
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(DividendSourceError::RateLimited(format!(
                "tiingo http {status}"
            )));
        }
        if !status.is_success() {
            return Err(DividendSourceError::Api(format!("tiingo http {status}")));
        }
        let body = response
            .text()
            .await
            .map_err(|e| DividendSourceError::Api(format!("tiingo body read failed: {e}")))?;

        parse_tiingo_dividends(ticker, since, &body)
    }
}

/// Parse a Tiingo daily-prices body into the dividend events with
/// `pay_date > since` (Tiingo's `date` is used as both ex- and pay-date, §7).
///
/// Pure (no I/O) so it is unit-tested against captured payloads. Rows with
/// `divCash <= 0` are non-dividend days and are skipped. A row with an
/// unparseable date or amount is skipped (best-effort, not a hard failure). A
/// body that is not valid Tiingo JSON is a typed [`DividendSourceError::Api`].
///
/// # Errors
/// [`DividendSourceError::Api`] if the body is not valid Tiingo daily-prices JSON.
fn parse_tiingo_dividends(
    ticker: &Ticker,
    since: NaiveDate,
    body: &str,
) -> Result<Vec<DividendEvent>, DividendSourceError> {
    let rows: Vec<TiingoDailyRow> = serde_json::from_str(body)
        .map_err(|e| DividendSourceError::Api(format!("tiingo decode failed: {e}")))?;
    let mut events = Vec::new();
    for row in rows {
        if row.div_cash <= 0.0 {
            continue;
        }
        // Tiingo dates are ISO-8601; take the leading YYYY-MM-DD.
        let Some(date_str) = row.date.get(0..10) else {
            continue;
        };
        let Ok(date) = NaiveDate::parse_from_str(date_str, "%Y-%m-%d") else {
            continue;
        };
        if date <= since {
            continue;
        }
        // f64 wire unit → exact Money via its string form (BUDGET-MONEY-1: f64 is
        // never the money TYPE, only the provider's wire unit).
        let Ok(amount) = Money::try_parse("tiingo_div_cash", &row.div_cash.to_string()) else {
            continue;
        };
        events.push(DividendEvent {
            ticker: ticker.clone(),
            ex_date: date,
            pay_date: date,
            amount_per_share: amount,
            source: DividendSourceKind::Tiingo,
        });
    }
    // Chronological by pay-date (Tiingo returns ascending, but make it explicit).
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

    // A captured-shape Tiingo daily-prices payload (date + divCash columns).
    const SAMPLE: &str = r#"[
        {"date":"2026-02-09T00:00:00.000Z","divCash":0.0},
        {"date":"2026-02-10T00:00:00.000Z","divCash":0.24},
        {"date":"2026-05-11T00:00:00.000Z","divCash":0.25}
    ]"#;

    #[test]
    fn parses_dividend_rows_skipping_zero_div_days() {
        let out = parse_tiingo_dividends(&ticker("AAPL"), date(2026, 1, 1), SAMPLE).unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].pay_date, date(2026, 2, 10));
        assert_eq!(
            out[0].amount_per_share,
            Money::try_parse("x", "0.24").unwrap()
        );
        assert_eq!(
            out[0].ex_date, out[0].pay_date,
            "ex == pay (best-effort §7)"
        );
        assert_eq!(out[0].source, DividendSourceKind::Tiingo);
        assert_eq!(out[1].pay_date, date(2026, 5, 11));
    }

    #[test]
    fn suppresses_rows_on_or_before_since() {
        // since = 2026-02-10 suppresses the 02-10 row (cutoff is exclusive).
        let out = parse_tiingo_dividends(&ticker("AAPL"), date(2026, 2, 10), SAMPLE).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].pay_date, date(2026, 5, 11));
    }

    #[test]
    fn empty_array_is_no_dividends() {
        let out = parse_tiingo_dividends(&ticker("AAPL"), date(2026, 1, 1), "[]").unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn garbage_body_is_a_typed_api_error() {
        let out = parse_tiingo_dividends(&ticker("AAPL"), date(2026, 1, 1), "not json");
        assert!(matches!(out, Err(DividendSourceError::Api(_))));
    }
}
