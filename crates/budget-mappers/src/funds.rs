//! Mapper: `budget-entities::funds::Model` ↔ `budget-domain::fund::Fund`.
//!
//! Conversions:
//!   - `id / user_id`: `Uuid` → typed IDs (`DOMAIN-2`)
//!   - `balance / target_balance`: entity `Decimal` / `Option<Decimal>` → `Money` (`BUDGET-MONEY-1`)
//!   - `kind`: entity `FundKind` → domain `FundKind` (1:1)
//!   - `created_at`: `DateTimeWithTimeZone` → `DateTime<Utc>` (`DOMAIN-7`)
//!
//! Total — no validated newtypes on `Fund`.

use chrono::Utc;
use sea_orm::ActiveValue::Set;

use budget_domain::enums::FundKind;
use budget_domain::fund::Fund;
use budget_domain::ids::{FundId, UserId};
use budget_domain::money::Money;

use budget_entities::funds;

use crate::MapperError;

fn kind_to_domain(e: funds::FundKind) -> FundKind {
    match e {
        funds::FundKind::Buffer => FundKind::Buffer,
        funds::FundKind::Surplus => FundKind::Surplus,
    }
}

fn kind_to_entity(d: FundKind) -> funds::FundKind {
    match d {
        FundKind::Buffer => funds::FundKind::Buffer,
        FundKind::Surplus => funds::FundKind::Surplus,
    }
}

/// Translate a `funds` [`funds::Model`] into a domain [`Fund`].
///
/// Total — no validated newtypes on `Fund`.
pub fn model_to_domain(m: funds::Model) -> Result<Fund, MapperError> {
    Ok(Fund {
        id: FundId::new(m.id),
        user_id: UserId::new(m.user_id),
        name: m.name,
        kind: kind_to_domain(m.kind),
        balance: Money::from_decimal(m.balance),
        target_balance: m.target_balance.map(Money::from_decimal),
        compulsory_repayment: m.compulsory_repayment,
        created_at: m.created_at.with_timezone(&Utc),
    })
}

/// Translate a domain [`Fund`] into a `funds` [`funds::ActiveModel`].
#[must_use]
pub fn domain_to_active_model(v: &Fund) -> funds::ActiveModel {
    funds::ActiveModel {
        id: Set(v.id.value()),
        user_id: Set(v.user_id.value()),
        name: Set(v.name.clone()),
        kind: Set(kind_to_entity(v.kind)),
        balance: Set(v.balance.as_decimal()),
        target_balance: Set(v.target_balance.map(|m| m.as_decimal())),
        compulsory_repayment: Set(v.compulsory_repayment),
        created_at: Set(v.created_at.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use rust_decimal::Decimal;
    use uuid::Uuid;

    fn buffer_model() -> funds::Model {
        funds::Model {
            id: Uuid::new_v4(),
            user_id: Uuid::new_v4(),
            name: "Emergency Buffer".to_owned(),
            kind: funds::FundKind::Buffer,
            balance: Decimal::new(500000, 2),     // $5000.00
            target_balance: Some(Decimal::new(500000, 2)), // $5000.00 target
            compulsory_repayment: true,
            created_at: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0)
                .unwrap()
                .into(),
        }
    }

    fn surplus_model() -> funds::Model {
        funds::Model {
            id: Uuid::new_v4(),
            user_id: Uuid::new_v4(),
            name: "MacBook Fund".to_owned(),
            kind: funds::FundKind::Surplus,
            balance: Decimal::new(60000, 2), // $600.00 saved so far
            target_balance: None,
            compulsory_repayment: false,
            created_at: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0)
                .unwrap()
                .into(),
        }
    }

    #[test]
    fn buffer_fund_round_trips() {
        let m = buffer_model();
        let domain = model_to_domain(m).unwrap_or_else(|_| unreachable!());
        assert_eq!(domain.kind, FundKind::Buffer);
        assert!(domain.compulsory_repayment);
        assert!(domain.target_balance.is_some());
        // At target — neither above nor below.
        assert!(!domain.is_above_target());
        assert!(!domain.is_below_target());
    }

    #[test]
    fn buffer_below_target_detected() {
        let mut m = buffer_model();
        m.balance = Decimal::new(300000, 2); // $3000 — below $5000 target
        let domain = model_to_domain(m).unwrap_or_else(|_| unreachable!());
        assert!(domain.is_below_target());
        assert!(!domain.is_above_target());
    }

    #[test]
    fn buffer_above_target_detected() {
        let mut m = buffer_model();
        m.balance = Decimal::new(600000, 2); // $6000 — above $5000 target
        let domain = model_to_domain(m).unwrap_or_else(|_| unreachable!());
        assert!(domain.is_above_target());
        assert!(!domain.is_below_target());
    }

    #[test]
    fn surplus_fund_no_target() {
        let m = surplus_model();
        let domain = model_to_domain(m).unwrap_or_else(|_| unreachable!());
        assert_eq!(domain.kind, FundKind::Surplus);
        assert!(!domain.compulsory_repayment);
        assert!(domain.target_balance.is_none());
        // No target → neither above nor below.
        assert!(!domain.is_above_target());
        assert!(!domain.is_below_target());
    }

    #[test]
    fn active_model_preserves_target_balance() {
        let m = buffer_model();
        let domain = model_to_domain(m).unwrap_or_else(|_| unreachable!());
        let am = domain_to_active_model(&domain);
        assert_eq!(
            am.target_balance,
            Set(domain.target_balance.map(|m| m.as_decimal()))
        );
    }
}
