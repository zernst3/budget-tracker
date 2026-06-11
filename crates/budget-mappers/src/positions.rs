//! Mapper: `budget-entities::positions::Model` ↔ `budget-domain::portfolio::Position`.
//!
//! Conversions:
//!   - `id / user_id`: `Uuid` → typed IDs (`DOMAIN-2`)
//!   - `ticker`: `String` → validated `Ticker` via `Ticker::try_new` — FALLIBLE.
//!     A stored value that fails validation signals data corruption and maps to
//!     [`MapperError::InvalidStoredValue`] (`field: "ticker"`).
//!   - `account_type` (entity `AccountType`) → domain `AccountType` (1:1)
//!   - `shares`: `Decimal` — a COUNT, passed through (`BUDGET-MONEY-1`: NOT money)
//!   - `cost_basis`: `Option<Decimal>` → `Option<Money>` (`BUDGET-MONEY-1`)
//!   - `created_at / updated_at`: `DateTimeWithTimeZone` → `DateTime<Utc>` (`DOMAIN-7`)

use chrono::{DateTime, Utc};
use sea_orm::ActiveValue::Set;

use budget_domain::enums::AccountType;
use budget_domain::ids::{PositionId, UserId};
use budget_domain::money::Money;
use budget_domain::portfolio::{Position, Ticker};

use budget_entities::accounts;
use budget_entities::positions;

use crate::MapperError;

// ---------------------------------------------------------------------------
// Entity enum → domain enum (positions reuses the accounts::AccountType pg-enum)
// ---------------------------------------------------------------------------

fn account_type_to_domain(e: accounts::AccountType) -> AccountType {
    match e {
        accounts::AccountType::Checking => AccountType::Checking,
        accounts::AccountType::Credit => AccountType::Credit,
        accounts::AccountType::Savings => AccountType::Savings,
        accounts::AccountType::Investment => AccountType::Investment,
        accounts::AccountType::Other => AccountType::Other,
    }
}

fn account_type_to_entity(d: AccountType) -> accounts::AccountType {
    match d {
        AccountType::Checking => accounts::AccountType::Checking,
        AccountType::Credit => accounts::AccountType::Credit,
        AccountType::Savings => accounts::AccountType::Savings,
        AccountType::Investment => accounts::AccountType::Investment,
        AccountType::Other => accounts::AccountType::Other,
    }
}

// ---------------------------------------------------------------------------
// Public mapper functions
// ---------------------------------------------------------------------------

/// Translate a stored `positions` [`positions::Model`] into a domain [`Position`].
///
/// FALLIBLE: the stored `ticker` is re-validated through [`Ticker::try_new`]; a
/// corrupt value surfaces as [`MapperError::InvalidStoredValue`].
///
/// # Errors
/// [`MapperError::InvalidStoredValue`] if the stored `ticker` fails validation.
pub fn model_to_domain(m: positions::Model) -> Result<Position, MapperError> {
    let ticker = Ticker::try_new(&m.ticker).map_err(|e| MapperError::InvalidStoredValue {
        field: "ticker",
        reason: e.to_string(),
    })?;
    Ok(Position {
        id: PositionId::new(m.id),
        user_id: UserId::new(m.user_id),
        ticker,
        account_label: m.account_label,
        account_type: account_type_to_domain(m.account_type),
        shares: m.shares,
        cost_basis: m.cost_basis.map(Money::from_decimal),
        created_at: m.created_at.with_timezone(&Utc),
        updated_at: m.updated_at.with_timezone(&Utc),
    })
}

/// Translate a domain [`Position`] into a `positions` [`positions::ActiveModel`].
#[must_use]
pub fn domain_to_active_model(v: &Position) -> positions::ActiveModel {
    let created_at: DateTime<Utc> = v.created_at;
    let updated_at: DateTime<Utc> = v.updated_at;
    positions::ActiveModel {
        id: Set(v.id.value()),
        user_id: Set(v.user_id.value()),
        ticker: Set(v.ticker.as_str().to_owned()),
        account_label: Set(v.account_label.clone()),
        account_type: Set(account_type_to_entity(v.account_type)),
        shares: Set(v.shares),
        cost_basis: Set(v.cost_basis.map(|m| m.as_decimal())),
        created_at: Set(created_at.into()),
        updated_at: Set(updated_at.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use rust_decimal::Decimal;
    use uuid::Uuid;

    fn sample_model(
        ticker: &str,
        cost_basis: Option<Decimal>,
        account_type: accounts::AccountType,
    ) -> positions::Model {
        let ts = Utc
            .with_ymd_and_hms(2026, 6, 10, 12, 0, 0)
            .single()
            .unwrap_or_else(Utc::now);
        positions::Model {
            id: Uuid::new_v4(),
            user_id: Uuid::new_v4(),
            ticker: ticker.to_owned(),
            account_label: "Fidelity Roth".to_owned(),
            account_type,
            shares: Decimal::new(10, 0),
            cost_basis,
            created_at: ts.into(),
            updated_at: ts.into(),
        }
    }

    #[test]
    fn investment_position_round_trips() {
        let m = sample_model(
            "AAPL",
            Some(Decimal::new(150000, 2)),
            accounts::AccountType::Investment,
        );
        let expected_id = m.id;
        let domain = model_to_domain(m).unwrap_or_else(|_| unreachable!());
        assert_eq!(domain.id.value(), expected_id);
        assert_eq!(domain.ticker.as_str(), "AAPL");
        assert_eq!(domain.account_type, AccountType::Investment);
        assert_eq!(domain.cost_basis, Some(Money::from_minor(150_000)));

        // ActiveModel preserves the values going back out.
        let am = domain_to_active_model(&domain);
        assert_eq!(am.ticker, Set("AAPL".to_owned()));
        assert_eq!(am.cost_basis, Set(Some(Decimal::new(150000, 2))));
        assert_eq!(am.id, Set(expected_id));
    }

    #[test]
    fn invalid_stored_ticker_is_rejected() {
        // A digit in the stored ticker fails Ticker::try_new (data corruption).
        let m = sample_model("AA1", None, accounts::AccountType::Investment);
        assert!(matches!(
            model_to_domain(m),
            Err(MapperError::InvalidStoredValue {
                field: "ticker",
                ..
            })
        ));
    }

    #[test]
    fn null_cost_basis_maps_to_none() {
        let m = sample_model("MSFT", None, accounts::AccountType::Investment);
        let domain = model_to_domain(m).unwrap_or_else(|_| unreachable!());
        assert_eq!(domain.cost_basis, None);
        let am = domain_to_active_model(&domain);
        assert_eq!(am.cost_basis, Set(None));
    }
}
