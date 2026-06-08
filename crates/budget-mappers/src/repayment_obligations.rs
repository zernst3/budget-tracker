//! Mapper: `budget-entities::repayment_obligations::Model`
//!         ↔ `budget-domain::repayment_obligation::RepaymentObligation`.
//!
//! Conversions:
//!   - `id / user_id / fund_id / transaction_id`: `Uuid` → typed IDs (`DOMAIN-2`)
//!   - `total_amount / remaining_amount / installment_amount`: entity `Decimal` → `Money` (`BUDGET-MONEY-1`)
//!   - `status`: entity `ObligationStatus` → domain `ObligationStatus` (1:1)
//!   - `created_at`: `DateTimeWithTimeZone` → `DateTime<Utc>` (`DOMAIN-7`)
//!
//! Total — no validated newtypes on `RepaymentObligation`.

use chrono::Utc;
use sea_orm::ActiveValue::Set;

use budget_domain::enums::ObligationStatus;
use budget_domain::ids::{FundId, RepaymentObligationId, TransactionId, UserId};
use budget_domain::money::Money;
use budget_domain::repayment_obligation::RepaymentObligation;

use budget_entities::repayment_obligations;

use crate::MapperError;

fn status_to_domain(e: repayment_obligations::ObligationStatus) -> ObligationStatus {
    match e {
        repayment_obligations::ObligationStatus::Active => ObligationStatus::Active,
        repayment_obligations::ObligationStatus::Paid => ObligationStatus::Paid,
    }
}

fn status_to_entity(d: ObligationStatus) -> repayment_obligations::ObligationStatus {
    match d {
        ObligationStatus::Active => repayment_obligations::ObligationStatus::Active,
        ObligationStatus::Paid => repayment_obligations::ObligationStatus::Paid,
    }
}

/// Translate a `repayment_obligations` [`repayment_obligations::Model`] into a
/// domain [`RepaymentObligation`].
///
/// Total — no validated newtypes on `RepaymentObligation`.
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
pub fn model_to_domain(
    m: repayment_obligations::Model,
) -> Result<RepaymentObligation, MapperError> {
    Ok(RepaymentObligation {
        id: RepaymentObligationId::new(m.id),
        user_id: UserId::new(m.user_id),
        fund_id: FundId::new(m.fund_id),
        transaction_id: TransactionId::new(m.transaction_id),
        total_amount: Money::from_decimal(m.total_amount),
        remaining_amount: Money::from_decimal(m.remaining_amount),
        installment_amount: Money::from_decimal(m.installment_amount),
        months_remaining: m.months_remaining,
        status: status_to_domain(m.status),
        created_at: m.created_at.with_timezone(&Utc),
    })
}

/// Translate a domain [`RepaymentObligation`] into a
/// `repayment_obligations` [`repayment_obligations::ActiveModel`].
#[must_use]
pub fn domain_to_active_model(v: &RepaymentObligation) -> repayment_obligations::ActiveModel {
    repayment_obligations::ActiveModel {
        id: Set(v.id.value()),
        user_id: Set(v.user_id.value()),
        fund_id: Set(v.fund_id.value()),
        transaction_id: Set(v.transaction_id.value()),
        total_amount: Set(v.total_amount.as_decimal()),
        remaining_amount: Set(v.remaining_amount.as_decimal()),
        installment_amount: Set(v.installment_amount.as_decimal()),
        months_remaining: Set(v.months_remaining),
        status: Set(status_to_entity(v.status)),
        created_at: Set(v.created_at.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use rust_decimal::Decimal;
    use uuid::Uuid;

    fn sample_model() -> repayment_obligations::Model {
        repayment_obligations::Model {
            id: Uuid::new_v4(),
            user_id: Uuid::new_v4(),
            fund_id: Uuid::new_v4(),
            transaction_id: Uuid::new_v4(),
            total_amount: Decimal::new(200_000, 2), // $2000.00 MacBook
            remaining_amount: Decimal::new(150_000, 2), // $1500.00 still owed
            installment_amount: Decimal::new(50000, 2), // $500.00/month
            months_remaining: 3,
            status: repayment_obligations::ObligationStatus::Active,
            created_at: Utc.with_ymd_and_hms(2026, 3, 1, 0, 0, 0).unwrap().into(),
        }
    }

    #[test]
    fn active_obligation_round_trips() {
        let m = sample_model();
        let expected_total = m.total_amount;
        let expected_remaining = m.remaining_amount;
        let domain = model_to_domain(m).unwrap_or_else(|_| unreachable!());
        assert_eq!(domain.status, ObligationStatus::Active);
        assert_eq!(domain.total_amount.as_decimal(), expected_total);
        assert_eq!(domain.remaining_amount.as_decimal(), expected_remaining);
        assert_eq!(domain.months_remaining, 3);
        assert!(!domain.is_settled());
    }

    #[test]
    fn paid_obligation_is_settled() {
        let mut m = sample_model();
        m.status = repayment_obligations::ObligationStatus::Paid;
        m.remaining_amount = Decimal::ZERO;
        m.months_remaining = 0;
        let domain = model_to_domain(m).unwrap_or_else(|_| unreachable!());
        assert_eq!(domain.status, ObligationStatus::Paid);
        assert!(domain.is_settled());
    }

    #[test]
    fn all_money_fields_preserve_precision() {
        let m = sample_model();
        let domain = model_to_domain(m).unwrap_or_else(|_| unreachable!());
        let am = domain_to_active_model(&domain);
        assert_eq!(am.total_amount, Set(domain.total_amount.as_decimal()));
        assert_eq!(
            am.remaining_amount,
            Set(domain.remaining_amount.as_decimal())
        );
        assert_eq!(
            am.installment_amount,
            Set(domain.installment_amount.as_decimal())
        );
    }

    #[test]
    fn fund_and_transaction_fks_preserved() {
        let m = sample_model();
        let expected_fund = m.fund_id;
        let expected_txn = m.transaction_id;
        let domain = model_to_domain(m).unwrap_or_else(|_| unreachable!());
        assert_eq!(domain.fund_id.value(), expected_fund);
        assert_eq!(domain.transaction_id.value(), expected_txn);
    }
}
