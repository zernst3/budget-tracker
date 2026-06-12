//! Mapper: `budget-entities::drip_applications::Model` ↔ `budget-domain::portfolio::DripApplication`.
//!
//! The append-only DRIP accretion chain (`SQL-AUDIT-COLUMNS-1`,
//! `BUDGET-ROLLOVER-INTEGRITY-1`). The domain [`DripApplication`] carries its own
//! `id`/`user_id`/`position_id`, so this mapper is a straight field translation
//! (mirrors [`crate::positions`]):
//!   - [`model_to_domain`] re-validates the stored `ticker` (fallible,
//!     `DOMAIN-3`); every other field passes through.
//!   - [`domain_to_active_model`] is total (every domain value is already valid).
//!
//! Conversions:
//!   - `id / user_id / position_id`: `Uuid` → typed IDs (`DOMAIN-2`)
//!   - `ticker`: `String` → validated `Ticker` (fallible)
//!   - `pay_date`: `Date` (`chrono::NaiveDate`) — passed through
//!   - `amount_per_share / price_used / cash_added`: `Decimal` → `Money` (`BUDGET-MONEY-1`)
//!   - `shares_added`: `Decimal` — a COUNT, passed through (`BUDGET-MONEY-1`)
//!   - `applied_at`: `DateTimeWithTimeZone` → `DateTime<Utc>` (`DOMAIN-7`)

use chrono::Utc;
use sea_orm::ActiveValue::Set;

use budget_domain::ids::{DripApplicationId, PositionId, UserId};
use budget_domain::money::Money;
use budget_domain::portfolio::{DripApplication, Ticker};

use budget_entities::drip_applications;

use crate::MapperError;

/// Translate a stored `drip_applications` [`drip_applications::Model`] into a
/// domain [`DripApplication`].
///
/// FALLIBLE: the stored `ticker` is re-validated through [`Ticker::try_new`].
///
/// # Errors
/// [`MapperError::InvalidStoredValue`] if the stored `ticker` fails validation.
pub fn model_to_domain(m: drip_applications::Model) -> Result<DripApplication, MapperError> {
    let ticker = Ticker::try_new(&m.ticker).map_err(|e| MapperError::InvalidStoredValue {
        field: "ticker",
        reason: e.to_string(),
    })?;
    Ok(DripApplication {
        id: DripApplicationId::new(m.id),
        user_id: UserId::new(m.user_id),
        position_id: PositionId::new(m.position_id),
        ticker,
        pay_date: m.pay_date,
        amount_per_share: Money::from_decimal(m.amount_per_share),
        price_used: Money::from_decimal(m.price_used),
        shares_added: m.shares_added,
        cash_added: Money::from_decimal(m.cash_added),
        drip_on_at_apply: m.drip_on_at_apply,
        applied_at: m.applied_at.with_timezone(&Utc),
    })
}

/// Translate a domain [`DripApplication`] into a `drip_applications`
/// [`drip_applications::ActiveModel`]. Total.
#[must_use]
pub fn domain_to_active_model(v: &DripApplication) -> drip_applications::ActiveModel {
    drip_applications::ActiveModel {
        id: Set(v.id.value()),
        user_id: Set(v.user_id.value()),
        position_id: Set(v.position_id.value()),
        ticker: Set(v.ticker.as_str().to_owned()),
        pay_date: Set(v.pay_date),
        amount_per_share: Set(v.amount_per_share.as_decimal()),
        price_used: Set(v.price_used.as_decimal()),
        shares_added: Set(v.shares_added),
        cash_added: Set(v.cash_added.as_decimal()),
        drip_on_at_apply: Set(v.drip_on_at_apply),
        applied_at: Set(v.applied_at.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{NaiveDate, TimeZone};
    use rust_decimal::Decimal;

    fn sample_domain() -> DripApplication {
        let ts = Utc
            .with_ymd_and_hms(2026, 5, 15, 12, 0, 0)
            .single()
            .unwrap_or_else(Utc::now);
        DripApplication {
            id: DripApplicationId::generate(),
            user_id: UserId::generate(),
            position_id: PositionId::generate(),
            ticker: Ticker::try_new("AAPL").unwrap_or_else(|_| unreachable!()),
            pay_date: NaiveDate::from_ymd_opt(2026, 5, 15).unwrap_or_default(),
            amount_per_share: Money::from_minor(25),
            price_used: Money::from_minor(18_000),
            shares_added: Decimal::new(125, 3),
            cash_added: Money::ZERO,
            drip_on_at_apply: true,
            applied_at: ts,
        }
    }

    /// Build a stored `Model` from a domain value WITHOUT round-tripping through
    /// the ActiveModel (so a per-field generic-closure type lock is avoided); the
    /// field values mirror `domain_to_active_model` exactly.
    fn model_from(v: &DripApplication) -> drip_applications::Model {
        drip_applications::Model {
            id: v.id.value(),
            user_id: v.user_id.value(),
            position_id: v.position_id.value(),
            ticker: v.ticker.as_str().to_owned(),
            pay_date: v.pay_date,
            amount_per_share: v.amount_per_share.as_decimal(),
            price_used: v.price_used.as_decimal(),
            shares_added: v.shares_added,
            cash_added: v.cash_added.as_decimal(),
            drip_on_at_apply: v.drip_on_at_apply,
            applied_at: v.applied_at.into(),
        }
    }

    #[test]
    fn drip_application_round_trips() {
        let original = sample_domain();
        // The ActiveModel carries the same values going out (spot-check a few).
        let am = domain_to_active_model(&original);
        assert_eq!(am.id, Set(original.id.value()));
        assert_eq!(am.shares_added, Set(Decimal::new(125, 3)));
        assert_eq!(am.drip_on_at_apply, Set(true));
        // The Model round-trips back to the domain value.
        let back = model_to_domain(model_from(&original)).unwrap_or_else(|_| unreachable!());
        assert_eq!(back, original);
    }

    #[test]
    fn invalid_stored_ticker_is_rejected() {
        let mut model = model_from(&sample_domain());
        model.ticker = "AA1".to_owned();
        assert!(matches!(
            model_to_domain(model),
            Err(MapperError::InvalidStoredValue {
                field: "ticker",
                ..
            })
        ));
    }
}
