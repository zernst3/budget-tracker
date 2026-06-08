//! Mapper: `budget-entities::months::Model` ↔ `budget-domain::month::Month`.
//!
//! Conversions:
//!   - `id / user_id / budget_id`: `Uuid` → typed IDs (`DOMAIN-2`)
//!   - `status`: entity `MonthStatus` → domain `MonthStatus` (1:1)
//!   - `opened_at / closed_at`: `DateTimeWithTimeZone` / `Option<DateTimeWithTimeZone>`
//!     → `DateTime<Utc>` / `Option<DateTime<Utc>>` via `.with_timezone(&Utc)` (`DOMAIN-7`)
//!
//! Total — no validated newtypes on `Month`.

use chrono::Utc;
use sea_orm::ActiveValue::Set;

use budget_domain::enums::MonthStatus;
use budget_domain::ids::{BudgetId, MonthId, UserId};
use budget_domain::month::Month;

use budget_entities::months;

use crate::MapperError;

fn status_to_domain(e: months::MonthStatus) -> MonthStatus {
    match e {
        months::MonthStatus::Open => MonthStatus::Open,
        months::MonthStatus::Closed => MonthStatus::Closed,
    }
}

fn status_to_entity(d: MonthStatus) -> months::MonthStatus {
    match d {
        MonthStatus::Open => months::MonthStatus::Open,
        MonthStatus::Closed => months::MonthStatus::Closed,
    }
}

/// Translate a `months` [`months::Model`] into a domain [`Month`].
///
/// Total — no validated newtypes on `Month`.
pub fn model_to_domain(m: months::Model) -> Result<Month, MapperError> {
    Ok(Month {
        id: MonthId::new(m.id),
        user_id: UserId::new(m.user_id),
        budget_id: BudgetId::new(m.budget_id),
        year: m.year,
        month: m.month,
        status: status_to_domain(m.status),
        opened_at: m.opened_at.with_timezone(&Utc),
        closed_at: m.closed_at.map(|dt| dt.with_timezone(&Utc)),
    })
}

/// Translate a domain [`Month`] into a `months` [`months::ActiveModel`].
#[must_use]
pub fn domain_to_active_model(v: &Month) -> months::ActiveModel {
    months::ActiveModel {
        id: Set(v.id.value()),
        user_id: Set(v.user_id.value()),
        budget_id: Set(v.budget_id.value()),
        year: Set(v.year),
        month: Set(v.month),
        status: Set(status_to_entity(v.status)),
        opened_at: Set(v.opened_at.into()),
        closed_at: Set(v.closed_at.map(Into::into)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use uuid::Uuid;

    fn sample_model() -> months::Model {
        months::Model {
            id: Uuid::new_v4(),
            user_id: Uuid::new_v4(),
            budget_id: Uuid::new_v4(),
            year: 2026,
            month: 6,
            status: months::MonthStatus::Open,
            opened_at: Utc.with_ymd_and_hms(2026, 6, 1, 4, 0, 0)
                .unwrap()
                .into(),
            closed_at: None,
        }
    }

    #[test]
    fn open_month_round_trips() {
        let m = sample_model();
        let expected_year = m.year;
        let expected_month = m.month;
        let domain = model_to_domain(m).unwrap_or_else(|_| unreachable!());
        assert_eq!(domain.year, expected_year);
        assert_eq!(domain.month, expected_month);
        assert_eq!(domain.status, MonthStatus::Open);
        assert!(domain.is_open());
        assert!(domain.closed_at.is_none());
    }

    #[test]
    fn closed_month_preserves_closed_at() {
        let mut m = sample_model();
        m.status = months::MonthStatus::Closed;
        let closed_ts = Utc.with_ymd_and_hms(2026, 7, 1, 4, 0, 0).unwrap();
        m.closed_at = Some(closed_ts.into());
        let domain = model_to_domain(m).unwrap_or_else(|_| unreachable!());
        assert_eq!(domain.status, MonthStatus::Closed);
        assert!(!domain.is_open());
        assert_eq!(domain.closed_at, Some(closed_ts));
    }

    #[test]
    fn sort_key_is_year_month_tuple() {
        let m = sample_model();
        let domain = model_to_domain(m).unwrap_or_else(|_| unreachable!());
        assert_eq!(domain.sort_key(), (2026, 6));
    }

    #[test]
    fn active_model_year_month_preserved() {
        let m = sample_model();
        let domain = model_to_domain(m).unwrap_or_else(|_| unreachable!());
        let am = domain_to_active_model(&domain);
        assert_eq!(am.year, Set(2026));
        assert_eq!(am.month, Set(6));
    }
}
