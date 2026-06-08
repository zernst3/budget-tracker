//! Mapper: `budget-entities::budgets::Model` ↔ `budget-domain::budget::Budget`.
//!
//! Conversions:
//!   - `id / user_id`: `Uuid` → `BudgetId` / `UserId` via `From<Uuid>` (`DOMAIN-2`)
//!   - `effective_from / effective_to`: `Date` (`NaiveDate`) — same type; pass through
//!   - `created_at: DateTimeWithTimeZone` → `DateTime<Utc>` via `.with_timezone(&Utc)` (`DOMAIN-7`)
//!
//! `model_to_domain` is total — no validated newtypes on `Budget`, so it cannot fail.
//! We still return `Result` for a uniform mapper interface.

use chrono::Utc;
use sea_orm::ActiveValue::Set;

use budget_domain::budget::Budget;
use budget_domain::ids::{BudgetId, UserId};

use budget_entities::budgets;

use crate::MapperError;

/// Translate a `budgets` [`budgets::Model`] into a domain [`Budget`].
///
/// Total — no validated newtypes on `Budget`; the `Result` wrapper keeps the
/// mapper interface uniform.
pub fn model_to_domain(m: budgets::Model) -> Result<Budget, MapperError> {
    Ok(Budget {
        id: BudgetId::new(m.id),
        user_id: UserId::new(m.user_id),
        name: m.name,
        effective_from: m.effective_from,
        effective_to: m.effective_to,
        created_at: m.created_at.with_timezone(&Utc),
    })
}

/// Translate a domain [`Budget`] into a `budgets` [`budgets::ActiveModel`].
#[must_use]
pub fn domain_to_active_model(v: &Budget) -> budgets::ActiveModel {
    budgets::ActiveModel {
        id: Set(v.id.value()),
        user_id: Set(v.user_id.value()),
        name: Set(v.name.clone()),
        effective_from: Set(v.effective_from),
        effective_to: Set(v.effective_to),
        created_at: Set(v.created_at.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{NaiveDate, TimeZone};
    use uuid::Uuid;

    fn sample_model() -> budgets::Model {
        budgets::Model {
            id: Uuid::new_v4(),
            user_id: Uuid::new_v4(),
            name: "NYC Budget".to_owned(),
            effective_from: NaiveDate::from_ymd_opt(2026, 1, 1).unwrap_or(NaiveDate::MIN),
            effective_to: None,
            created_at: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0)
                .unwrap()
                .into(),
        }
    }

    #[test]
    fn round_trip_preserves_fields() {
        let m = sample_model();
        let expected_id = m.id;
        let expected_name = m.name.clone();
        let domain = model_to_domain(m).unwrap_or_else(|_| unreachable!());
        assert_eq!(domain.id.value(), expected_id);
        assert_eq!(domain.name, expected_name);
        assert!(domain.is_current());
    }

    #[test]
    fn active_model_id_matches() {
        let m = sample_model();
        let domain = model_to_domain(m).unwrap_or_else(|_| unreachable!());
        let am = domain_to_active_model(&domain);
        assert_eq!(am.id, Set(domain.id.value()));
        assert_eq!(am.name, Set(domain.name.clone()));
    }
}
