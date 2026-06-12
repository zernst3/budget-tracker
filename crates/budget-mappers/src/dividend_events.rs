//! Mapper: `budget-entities::dividend_events::Model` ↔ `budget-domain::portfolio::DividendEvent`.
//!
//! The domain [`DividendEvent`] is identity-free (no `id`, no `fetched_at`):
//! those are persistence concerns supplied at write time, not part of the domain
//! shape (mirrors the [`crate::cash_balances`] mapper). So:
//!   - [`model_to_domain`] projects the entity down to the five domain fields,
//!     dropping `id`/`fetched_at`. FALLIBLE: the stored `ticker` is re-validated
//!     through [`Ticker::try_new`] and the stored `source` through
//!     [`DividendSourceKind::try_from_str`]; a corrupt value surfaces as
//!     [`MapperError::InvalidStoredValue`] (`DOMAIN-3` at the storage boundary).
//!   - [`to_active_model`] takes the persistence context (`id`, `fetched_at`)
//!     explicitly alongside the domain value.
//!
//! Conversions:
//!   - `ticker`: `String` → validated `Ticker` (fallible)
//!   - `source`: `String` → `DividendSourceKind` (fallible)
//!   - `amount_per_share`: `Decimal` → `Money` (`BUDGET-MONEY-1`)
//!   - `ex_date / pay_date`: `Date` (`chrono::NaiveDate`) — passed through

use chrono::{DateTime, NaiveDate, Utc};
use sea_orm::ActiveValue::Set;

use budget_domain::money::Money;
use budget_domain::portfolio::{DividendEvent, DividendSourceKind, Ticker};

use budget_entities::dividend_events;

use crate::MapperError;

/// Translate a stored `dividend_events` [`dividend_events::Model`] into a domain
/// [`DividendEvent`], dropping the persistence-only columns (`id`, `fetched_at`).
///
/// FALLIBLE: the stored `ticker` and `source` are re-validated; a corrupt value
/// surfaces as [`MapperError::InvalidStoredValue`].
///
/// # Errors
/// [`MapperError::InvalidStoredValue`] if the stored `ticker` or `source` fails
/// validation.
pub fn model_to_domain(m: dividend_events::Model) -> Result<DividendEvent, MapperError> {
    let ticker = Ticker::try_new(&m.ticker).map_err(|e| MapperError::InvalidStoredValue {
        field: "ticker",
        reason: e.to_string(),
    })?;
    let source = DividendSourceKind::try_from_str(&m.source).map_err(|e| {
        MapperError::InvalidStoredValue {
            field: "dividend_source",
            reason: e.to_string(),
        }
    })?;
    Ok(DividendEvent {
        ticker,
        ex_date: m.ex_date,
        pay_date: m.pay_date,
        amount_per_share: Money::from_decimal(m.amount_per_share),
        source,
    })
}

/// Translate a domain [`DividendEvent`] into a `dividend_events`
/// [`dividend_events::ActiveModel`], supplying the persistence context (`id`,
/// `fetched_at`) the domain value does not carry.
#[must_use]
pub fn to_active_model(
    id: uuid::Uuid,
    v: &DividendEvent,
    fetched_at: DateTime<Utc>,
) -> dividend_events::ActiveModel {
    let ex_date: NaiveDate = v.ex_date;
    let pay_date: NaiveDate = v.pay_date;
    dividend_events::ActiveModel {
        id: Set(id),
        ticker: Set(v.ticker.as_str().to_owned()),
        ex_date: Set(ex_date),
        pay_date: Set(pay_date),
        amount_per_share: Set(v.amount_per_share.as_decimal()),
        source: Set(v.source.as_str().to_owned()),
        fetched_at: Set(fetched_at.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use rust_decimal::Decimal;
    use uuid::Uuid;

    fn sample_model(ticker: &str, source: &str) -> dividend_events::Model {
        let ts = Utc
            .with_ymd_and_hms(2026, 5, 15, 12, 0, 0)
            .single()
            .unwrap_or_else(Utc::now);
        dividend_events::Model {
            id: Uuid::new_v4(),
            ticker: ticker.to_owned(),
            ex_date: NaiveDate::from_ymd_opt(2026, 5, 8).unwrap_or_default(),
            pay_date: NaiveDate::from_ymd_opt(2026, 5, 15).unwrap_or_default(),
            amount_per_share: Decimal::new(25, 2),
            source: source.to_owned(),
            fetched_at: ts.into(),
        }
    }

    #[test]
    fn dividend_event_round_trips() {
        let m = sample_model("AAPL", "tiingo");
        let domain = model_to_domain(m).unwrap_or_else(|_| unreachable!());
        assert_eq!(domain.ticker.as_str(), "AAPL");
        assert_eq!(domain.source, DividendSourceKind::Tiingo);
        assert_eq!(domain.amount_per_share, Money::from_minor(25));

        let now = Utc::now();
        let id = Uuid::new_v4();
        let am = to_active_model(id, &domain, now);
        assert_eq!(am.ticker, Set("AAPL".to_owned()));
        assert_eq!(am.source, Set("tiingo".to_owned()));
        assert_eq!(am.amount_per_share, Set(Decimal::new(25, 2)));
        assert_eq!(am.id, Set(id));
    }

    #[test]
    fn invalid_stored_ticker_is_rejected() {
        let m = sample_model("AA1", "tiingo");
        assert!(matches!(
            model_to_domain(m),
            Err(MapperError::InvalidStoredValue {
                field: "ticker",
                ..
            })
        ));
    }

    #[test]
    fn invalid_stored_source_is_rejected() {
        let m = sample_model("AAPL", "alphavantage");
        assert!(matches!(
            model_to_domain(m),
            Err(MapperError::InvalidStoredValue {
                field: "dividend_source",
                ..
            })
        ));
    }
}
