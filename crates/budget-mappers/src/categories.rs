//! Mapper: `budget-entities::categories::Model` ↔ `budget-domain::category::Category`.
//!
//! Conversions:
//!   - `id / budget_id / category_key`: `Uuid` → typed IDs (`DOMAIN-2`)
//!   - `amount / fund_balance`: entity `Decimal` → `Money::from_decimal` (`BUDGET-MONEY-1`)
//!   - `grp`: entity `CategoryGrp` → domain `CategoryGrp` (1:1 variant names)
//!   - `settle_type`: entity `Option<SettleType>` → domain `Option<SettleType>` (1:1)
//!   - `cadence`: entity `Cadence` → domain `Cadence` (1:1)
//!   - `next_due_date`: `Option<Date>` (`NaiveDate`) — same type; pass through
//!   - No timestamp columns on `Category`; no `DOMAIN-7` conversion needed.
//!
//! Total — no validated newtypes on `Category`.

use sea_orm::ActiveValue::Set;

use budget_domain::category::Category;
use budget_domain::enums::{Cadence, CategoryGrp, SettleType};
use budget_domain::ids::{BudgetId, CategoryId, CategoryKey};
use budget_domain::money::Money;

use budget_entities::categories;

use crate::MapperError;

// ---------------------------------------------------------------------------
// Entity enum → domain enum
// ---------------------------------------------------------------------------

fn grp_to_domain(e: categories::CategoryGrp) -> CategoryGrp {
    match e {
        categories::CategoryGrp::Fixed => CategoryGrp::Fixed,
        categories::CategoryGrp::Discretionary => CategoryGrp::Discretionary,
    }
}

fn grp_to_entity(d: CategoryGrp) -> categories::CategoryGrp {
    match d {
        CategoryGrp::Fixed => categories::CategoryGrp::Fixed,
        CategoryGrp::Discretionary => categories::CategoryGrp::Discretionary,
    }
}

fn settle_to_domain(e: categories::SettleType) -> SettleType {
    match e {
        categories::SettleType::TrueSet => SettleType::TrueSet,
        categories::SettleType::FlexibleSet => SettleType::FlexibleSet,
    }
}

fn settle_to_entity(d: SettleType) -> categories::SettleType {
    match d {
        SettleType::TrueSet => categories::SettleType::TrueSet,
        SettleType::FlexibleSet => categories::SettleType::FlexibleSet,
    }
}

fn cadence_to_domain(e: categories::Cadence) -> Cadence {
    match e {
        categories::Cadence::Monthly => Cadence::Monthly,
        categories::Cadence::Quarterly => Cadence::Quarterly,
        categories::Cadence::Semiannual => Cadence::Semiannual,
        categories::Cadence::Annual => Cadence::Annual,
    }
}

fn cadence_to_entity(d: Cadence) -> categories::Cadence {
    match d {
        Cadence::Monthly => categories::Cadence::Monthly,
        Cadence::Quarterly => categories::Cadence::Quarterly,
        Cadence::Semiannual => categories::Cadence::Semiannual,
        Cadence::Annual => categories::Cadence::Annual,
    }
}

// ---------------------------------------------------------------------------
// Public mapper functions
// ---------------------------------------------------------------------------

/// Translate a `categories` [`categories::Model`] into a domain [`Category`].
///
/// Total — no validated newtypes on `Category`.
///
/// # Errors
///
/// Currently infallible; returns `Result` for a uniform mapper signature
/// (`MAPPER-1`) so every read-path entry point composes identically once
/// fallible aggregates are added.
pub fn model_to_domain(m: categories::Model) -> Result<Category, MapperError> {
    Ok(Category {
        id: CategoryId::new(m.id),
        budget_id: BudgetId::new(m.budget_id),
        category_key: CategoryKey::new(m.category_key),
        name: m.name,
        amount: Money::from_decimal(m.amount),
        grp: grp_to_domain(m.grp),
        settle_type: m.settle_type.map(settle_to_domain),
        expected_bills: m.expected_bills,
        is_rollover_bucket: m.is_rollover_bucket,
        cadence: cadence_to_domain(m.cadence),
        period_months: m.period_months,
        fund_balance: Money::from_decimal(m.fund_balance),
        next_due_date: m.next_due_date,
        sort_order: m.sort_order,
    })
}

/// Translate a domain [`Category`] into a `categories` [`categories::ActiveModel`].
#[must_use]
pub fn domain_to_active_model(v: &Category) -> categories::ActiveModel {
    categories::ActiveModel {
        id: Set(v.id.value()),
        budget_id: Set(v.budget_id.value()),
        category_key: Set(v.category_key.value()),
        name: Set(v.name.clone()),
        amount: Set(v.amount.as_decimal()),
        grp: Set(grp_to_entity(v.grp)),
        settle_type: Set(v.settle_type.map(settle_to_entity)),
        expected_bills: Set(v.expected_bills),
        is_rollover_bucket: Set(v.is_rollover_bucket),
        cadence: Set(cadence_to_entity(v.cadence)),
        period_months: Set(v.period_months),
        fund_balance: Set(v.fund_balance.as_decimal()),
        next_due_date: Set(v.next_due_date),
        sort_order: Set(v.sort_order),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::Decimal;
    use uuid::Uuid;

    fn sample_model() -> categories::Model {
        categories::Model {
            id: Uuid::new_v4(),
            budget_id: Uuid::new_v4(),
            category_key: Uuid::new_v4(),
            name: "Rent".to_owned(),
            amount: Decimal::new(250_000, 2), // $2500.00
            grp: categories::CategoryGrp::Fixed,
            settle_type: Some(categories::SettleType::TrueSet),
            expected_bills: None,
            is_rollover_bucket: false,
            cadence: categories::Cadence::Monthly,
            period_months: None,
            fund_balance: Decimal::ZERO,
            next_due_date: None,
            sort_order: 1,
        }
    }

    #[test]
    fn round_trip_fixed_category() {
        let m = sample_model();
        let expected_id = m.id;
        let expected_amount = m.amount;
        let domain = model_to_domain(m).unwrap_or_else(|_| unreachable!());
        assert_eq!(domain.id.value(), expected_id);
        assert_eq!(domain.amount.as_decimal(), expected_amount);
        assert_eq!(domain.grp, CategoryGrp::Fixed);
        assert_eq!(domain.settle_type, Some(SettleType::TrueSet));
        assert!(!domain.is_rollover_bucket);
        assert!(!domain.is_sinking_fund());
    }

    #[test]
    fn sinking_fund_category_round_trips() {
        let mut m = sample_model();
        m.cadence = categories::Cadence::Annual;
        m.amount = Decimal::new(12000, 2); // $120.00/yr -> $10.00/mo accrual
        let domain = model_to_domain(m).unwrap_or_else(|_| unreachable!());
        assert!(domain.is_sinking_fund());
        // accrual_per_month = $120 / 12 = $10
        let accrual = domain.accrual_per_month();
        assert_eq!(accrual.as_decimal(), Decimal::new(1000, 2));
    }

    #[test]
    fn rollover_bucket_flag_preserved() {
        let mut m = sample_model();
        m.is_rollover_bucket = true;
        m.grp = categories::CategoryGrp::Discretionary;
        m.settle_type = None;
        let domain = model_to_domain(m).unwrap_or_else(|_| unreachable!());
        assert!(domain.is_rollover_bucket);
        assert_eq!(domain.grp, CategoryGrp::Discretionary);
        assert!(domain.settle_type.is_none());
    }

    #[test]
    fn active_model_preserves_money_precision() {
        let m = sample_model();
        let domain = model_to_domain(m).unwrap_or_else(|_| unreachable!());
        let am = domain_to_active_model(&domain);
        assert_eq!(am.amount, Set(domain.amount.as_decimal()));
        assert_eq!(am.fund_balance, Set(domain.fund_balance.as_decimal()));
    }
}
