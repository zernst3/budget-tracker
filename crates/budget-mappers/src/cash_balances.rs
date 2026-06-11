//! Mapper: `budget-entities::cash_balances::Model` ↔ `budget-domain::portfolio::CashBalance`.
//!
//! The domain [`CashBalance`] is a thin value (no `id`, no `user_id`, no
//! timestamps): those columns are persistence concerns supplied at write time,
//! not part of the domain shape. So:
//!   - [`model_to_domain`] projects the entity down to the three domain fields
//!     (`account_label`, `balance`, `reserved`), dropping the persistence
//!     columns. Total — no validated newtypes on `CashBalance` — but returns
//!     `Result` for a uniform mapper signature (`MAPPER-1`).
//!   - [`to_active_model`] takes the persistence context (`id`, `user_id`, `now`)
//!     explicitly alongside the domain value.
//!
//! Conversions:
//!   - `balance`: `Decimal` → `Money::from_decimal` / `.as_decimal()` (`BUDGET-MONEY-1`)
//!   - `reserved`: `bool` — pass through (`BUDGET-CASH-1`)
//!   - `created_at / updated_at`: `DateTimeWithTimeZone` (`now.into()` going out)

use chrono::{DateTime, Utc};
use sea_orm::ActiveValue::Set;

use budget_domain::ids::UserId;
use budget_domain::money::Money;
use budget_domain::portfolio::CashBalance;

use budget_entities::cash_balances;

use crate::MapperError;

/// Translate a stored `cash_balances` [`cash_balances::Model`] into a domain
/// [`CashBalance`], dropping the persistence-only columns (`id`, `user_id`,
/// timestamps).
///
/// Total — no validated newtypes on `CashBalance`.
///
/// # Errors
/// Currently infallible; returns `Result` for a uniform mapper signature
/// (`MAPPER-1`).
pub fn model_to_domain(m: cash_balances::Model) -> Result<CashBalance, MapperError> {
    Ok(CashBalance {
        account_label: m.account_label,
        balance: Money::from_decimal(m.balance),
        reserved: m.reserved,
    })
}

/// Translate a domain [`CashBalance`] into a `cash_balances`
/// [`cash_balances::ActiveModel`], supplying the persistence context (`id`,
/// `user_id`, `now`) the domain value does not carry.
#[must_use]
pub fn to_active_model(
    id: uuid::Uuid,
    user_id: UserId,
    v: &CashBalance,
    now: DateTime<Utc>,
) -> cash_balances::ActiveModel {
    cash_balances::ActiveModel {
        id: Set(id),
        user_id: Set(user_id.value()),
        account_label: Set(v.account_label.clone()),
        balance: Set(v.balance.as_decimal()),
        reserved: Set(v.reserved),
        created_at: Set(now.into()),
        updated_at: Set(now.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use rust_decimal::Decimal;
    use uuid::Uuid;

    fn sample_model(reserved: bool) -> cash_balances::Model {
        let ts = Utc
            .with_ymd_and_hms(2026, 6, 10, 12, 0, 0)
            .single()
            .unwrap_or_else(|| Utc::now());
        cash_balances::Model {
            id: Uuid::new_v4(),
            user_id: Uuid::new_v4(),
            account_label: "Emergency Fund".to_owned(),
            balance: Decimal::new(500000, 2),
            reserved,
            created_at: ts.into(),
            updated_at: ts.into(),
        }
    }

    #[test]
    fn reserved_balance_round_trips() {
        let m = sample_model(true);
        let domain = model_to_domain(m).unwrap_or_else(|_| unreachable!());
        assert_eq!(domain.account_label, "Emergency Fund");
        assert_eq!(domain.balance, Money::from_minor(500_000));
        assert!(domain.reserved);

        let now = Utc
            .with_ymd_and_hms(2026, 6, 11, 0, 0, 0)
            .single()
            .unwrap_or_else(|| Utc::now());
        let id = Uuid::new_v4();
        let user_id = UserId::generate();
        let am = to_active_model(id, user_id, &domain, now);
        assert_eq!(am.balance, Set(Decimal::new(500000, 2)));
        assert_eq!(am.reserved, Set(true));
        assert_eq!(am.id, Set(id));
        assert_eq!(am.user_id, Set(user_id.value()));
    }

    #[test]
    fn non_reserved_balance_maps_correctly() {
        let m = sample_model(false);
        let domain = model_to_domain(m).unwrap_or_else(|_| unreachable!());
        assert!(!domain.reserved);
        let now = Utc::now();
        let am = to_active_model(Uuid::new_v4(), UserId::generate(), &domain, now);
        assert_eq!(am.reserved, Set(false));
    }
}
