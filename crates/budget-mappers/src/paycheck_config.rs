//! Mapper: `budget-entities::paycheck_config::Model`
//!         ↔ `budget-domain::paycheck_config::PaycheckConfig`.
//!
//! Conversions:
//!   - `id / user_id`: `Uuid` → typed IDs (`DOMAIN-2`)
//!   - `amount / smoothing_buffer`: entity `Option<Decimal>` / `Decimal` → `Option<Money>` / `Money`
//!     (`BUDGET-MONEY-1`)
//!   - `income_mode / paycheck_type / surplus_routing`: entity enums → domain enums (1:1)
//!   - `anchor_date`: `Date` (`NaiveDate`) — same type; pass through
//!   - No timestamp columns on `PaycheckConfig` (schema has no `created_at`).
//!
//! Total — no validated newtypes on `PaycheckConfig`.

use sea_orm::ActiveValue::Set;

use budget_domain::enums::{IncomeMode, PaycheckType, SurplusRouting};
use budget_domain::ids::{PaycheckConfigId, UserId};
use budget_domain::money::Money;
use budget_domain::paycheck_config::PaycheckConfig;

use budget_entities::paycheck_config;

use crate::MapperError;

// ---------------------------------------------------------------------------
// Entity enum → domain enum
// ---------------------------------------------------------------------------

fn income_mode_to_domain(e: paycheck_config::IncomeMode) -> IncomeMode {
    match e {
        paycheck_config::IncomeMode::PerPaycheck => IncomeMode::PerPaycheck,
        paycheck_config::IncomeMode::Smoothed => IncomeMode::Smoothed,
    }
}

fn income_mode_to_entity(d: IncomeMode) -> paycheck_config::IncomeMode {
    match d {
        IncomeMode::PerPaycheck => paycheck_config::IncomeMode::PerPaycheck,
        IncomeMode::Smoothed => paycheck_config::IncomeMode::Smoothed,
    }
}

fn paycheck_type_to_domain(e: paycheck_config::PaycheckType) -> PaycheckType {
    match e {
        paycheck_config::PaycheckType::Semimonthly => PaycheckType::Semimonthly,
        paycheck_config::PaycheckType::Biweekly => PaycheckType::Biweekly,
        paycheck_config::PaycheckType::Weekly => PaycheckType::Weekly,
        paycheck_config::PaycheckType::Hourly => PaycheckType::Hourly,
    }
}

fn paycheck_type_to_entity(d: PaycheckType) -> paycheck_config::PaycheckType {
    match d {
        PaycheckType::Semimonthly => paycheck_config::PaycheckType::Semimonthly,
        PaycheckType::Biweekly => paycheck_config::PaycheckType::Biweekly,
        PaycheckType::Weekly => paycheck_config::PaycheckType::Weekly,
        PaycheckType::Hourly => paycheck_config::PaycheckType::Hourly,
    }
}

fn surplus_routing_to_domain(e: paycheck_config::SurplusRouting) -> SurplusRouting {
    match e {
        paycheck_config::SurplusRouting::Buffer => SurplusRouting::Buffer,
        paycheck_config::SurplusRouting::ThisMonth => SurplusRouting::ThisMonth,
        paycheck_config::SurplusRouting::Savings => SurplusRouting::Savings,
    }
}

fn surplus_routing_to_entity(d: SurplusRouting) -> paycheck_config::SurplusRouting {
    match d {
        SurplusRouting::Buffer => paycheck_config::SurplusRouting::Buffer,
        SurplusRouting::ThisMonth => paycheck_config::SurplusRouting::ThisMonth,
        SurplusRouting::Savings => paycheck_config::SurplusRouting::Savings,
    }
}

// ---------------------------------------------------------------------------
// Public mapper functions
// ---------------------------------------------------------------------------

/// Translate a `paycheck_config` [`paycheck_config::Model`] into a domain
/// [`PaycheckConfig`].
///
/// Total — no validated newtypes on `PaycheckConfig`.
///
/// # Errors
///
/// Currently infallible; returns `Result` for a uniform mapper signature
/// (`MAPPER-1`) so every read-path entry point composes identically once
/// fallible aggregates are added.
// The owned `Model` is intentionally consumed: this is the read-path entry
// point and callers hand off the just-fetched row. Every field is `Copy`, so
// clippy sees no move, but the ownership contract is deliberate.
#[allow(clippy::needless_pass_by_value)]
pub fn model_to_domain(m: paycheck_config::Model) -> Result<PaycheckConfig, MapperError> {
    Ok(PaycheckConfig {
        id: PaycheckConfigId::new(m.id),
        user_id: UserId::new(m.user_id),
        income_mode: income_mode_to_domain(m.income_mode),
        paycheck_type: paycheck_type_to_domain(m.paycheck_type),
        amount: m.amount.map(Money::from_decimal),
        anchor_date: m.anchor_date,
        surplus_routing: surplus_routing_to_domain(m.surplus_routing),
        smoothing_buffer: Money::from_decimal(m.smoothing_buffer),
    })
}

/// Translate a domain [`PaycheckConfig`] into a `paycheck_config`
/// [`paycheck_config::ActiveModel`].
#[must_use]
pub fn domain_to_active_model(v: &PaycheckConfig) -> paycheck_config::ActiveModel {
    paycheck_config::ActiveModel {
        id: Set(v.id.value()),
        user_id: Set(v.user_id.value()),
        income_mode: Set(income_mode_to_entity(v.income_mode)),
        paycheck_type: Set(paycheck_type_to_entity(v.paycheck_type)),
        amount: Set(v.amount.map(|m| m.as_decimal())),
        anchor_date: Set(v.anchor_date),
        surplus_routing: Set(surplus_routing_to_entity(v.surplus_routing)),
        smoothing_buffer: Set(v.smoothing_buffer.as_decimal()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;
    use rust_decimal::Decimal;
    use uuid::Uuid;

    fn semimonthly_model() -> paycheck_config::Model {
        paycheck_config::Model {
            id: Uuid::new_v4(),
            user_id: Uuid::new_v4(),
            income_mode: paycheck_config::IncomeMode::PerPaycheck,
            paycheck_type: paycheck_config::PaycheckType::Semimonthly,
            amount: Some(Decimal::new(350_000, 2)), // $3500.00 per paycheck
            anchor_date: NaiveDate::from_ymd_opt(2026, 6, 15).unwrap_or(NaiveDate::MIN),
            surplus_routing: paycheck_config::SurplusRouting::Buffer,
            smoothing_buffer: Decimal::ZERO,
        }
    }

    fn hourly_model() -> paycheck_config::Model {
        paycheck_config::Model {
            id: Uuid::new_v4(),
            user_id: Uuid::new_v4(),
            income_mode: paycheck_config::IncomeMode::PerPaycheck,
            paycheck_type: paycheck_config::PaycheckType::Hourly,
            amount: None, // hourly: no fixed amount
            anchor_date: NaiveDate::from_ymd_opt(2026, 6, 15).unwrap_or(NaiveDate::MIN),
            surplus_routing: paycheck_config::SurplusRouting::ThisMonth,
            smoothing_buffer: Decimal::ZERO,
        }
    }

    #[test]
    fn semimonthly_config_round_trips() {
        let m = semimonthly_model();
        let expected_amount = m.amount;
        let domain = model_to_domain(m).unwrap_or_else(|_| unreachable!());
        assert_eq!(domain.income_mode, IncomeMode::PerPaycheck);
        assert_eq!(domain.paycheck_type, PaycheckType::Semimonthly);
        assert_eq!(domain.amount.map(|a| a.as_decimal()), expected_amount);
        // Semimonthly = 24/yr
        assert_eq!(domain.paycheck_type.paychecks_per_year(), Some(24));
    }

    #[test]
    fn hourly_config_has_no_amount() {
        let m = hourly_model();
        let domain = model_to_domain(m).unwrap_or_else(|_| unreachable!());
        assert_eq!(domain.paycheck_type, PaycheckType::Hourly);
        assert!(domain.amount.is_none());
        assert_eq!(domain.paycheck_type.paychecks_per_year(), None);
    }

    #[test]
    fn all_surplus_routings_map() {
        for (entity_routing, expected) in [
            (
                paycheck_config::SurplusRouting::Buffer,
                SurplusRouting::Buffer,
            ),
            (
                paycheck_config::SurplusRouting::ThisMonth,
                SurplusRouting::ThisMonth,
            ),
            (
                paycheck_config::SurplusRouting::Savings,
                SurplusRouting::Savings,
            ),
        ] {
            let mut m = semimonthly_model();
            m.surplus_routing = entity_routing;
            let domain = model_to_domain(m).unwrap_or_else(|_| unreachable!());
            assert_eq!(domain.surplus_routing, expected);
        }
    }

    #[test]
    fn active_model_preserves_smoothing_buffer() {
        let m = semimonthly_model();
        let domain = model_to_domain(m).unwrap_or_else(|_| unreachable!());
        let am = domain_to_active_model(&domain);
        assert_eq!(
            am.smoothing_buffer,
            Set(domain.smoothing_buffer.as_decimal())
        );
    }

    #[test]
    fn all_paycheck_types_map() {
        for (entity_type, expected) in [
            (
                paycheck_config::PaycheckType::Semimonthly,
                PaycheckType::Semimonthly,
            ),
            (
                paycheck_config::PaycheckType::Biweekly,
                PaycheckType::Biweekly,
            ),
            (paycheck_config::PaycheckType::Weekly, PaycheckType::Weekly),
            (paycheck_config::PaycheckType::Hourly, PaycheckType::Hourly),
        ] {
            let mut m = semimonthly_model();
            m.paycheck_type = entity_type;
            let domain = model_to_domain(m).unwrap_or_else(|_| unreachable!());
            assert_eq!(domain.paycheck_type, expected);
        }
    }
}
