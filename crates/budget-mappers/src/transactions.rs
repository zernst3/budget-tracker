//! Mapper: `budget-entities::transactions::Model` ↔ `budget-domain::transaction::Transaction`.
//!
//! **Two** public `model_to_domain` functions:
//!
//! - [`model_to_domain`]: for rows already stored in the DB with the internal sign
//!   convention (negative = expense). Called when reading back stored rows.
//! - [`plaid_model_to_domain`]: for rows freshly mapped from Plaid before insert.
//!   Plaid reports positive = outflow; this function flips the sign once at the
//!   mapper boundary (`BUDGET-PLAID-SIGN-1`) and includes a runtime direction test
//!   to catch future Plaid API changes early. ONLY this function handles Plaid-sign
//!   normalization; no downstream code flips signs.
//!
//! Conversions:
//!   - `id / user_id / month_id / category_id / account_id`: `Uuid` / `Option<Uuid>` → IDs
//!   - `amount`: entity `Decimal` → `Money::from_decimal` (`BUDGET-MONEY-1`)
//!   - `source / status / income_kind`: entity enum → domain enum (1:1)
//!   - `date`: `Date` (`NaiveDate`) — same type; pass through
//!   - `created_at / updated_at`: `DateTimeWithTimeZone` → `DateTime<Utc>` (`DOMAIN-7`)

use chrono::{DateTime, Utc};
use sea_orm::ActiveValue::Set;

use budget_domain::enums::{IncomeKind, TransactionSource, TransactionStatus};
use budget_domain::ids::{AccountId, CategoryId, MonthId, TransactionId, UserId};
use budget_domain::money::Money;
use budget_domain::plaid_api::PlaidTransaction;
use budget_domain::transaction::Transaction;

use budget_entities::transactions;

use crate::MapperError;

// ---------------------------------------------------------------------------
// Entity enum → domain enum
// ---------------------------------------------------------------------------

fn source_to_domain(e: transactions::TransactionSource) -> TransactionSource {
    match e {
        transactions::TransactionSource::Manual => TransactionSource::Manual,
        transactions::TransactionSource::Plaid => TransactionSource::Plaid,
    }
}

fn source_to_entity(d: TransactionSource) -> transactions::TransactionSource {
    match d {
        TransactionSource::Manual => transactions::TransactionSource::Manual,
        TransactionSource::Plaid => transactions::TransactionSource::Plaid,
    }
}

fn status_to_domain(e: transactions::TransactionStatus) -> TransactionStatus {
    match e {
        transactions::TransactionStatus::Pending => TransactionStatus::Pending,
        transactions::TransactionStatus::Settled => TransactionStatus::Settled,
        transactions::TransactionStatus::Expected => TransactionStatus::Expected,
    }
}

fn status_to_entity(d: TransactionStatus) -> transactions::TransactionStatus {
    match d {
        TransactionStatus::Pending => transactions::TransactionStatus::Pending,
        TransactionStatus::Settled => transactions::TransactionStatus::Settled,
        TransactionStatus::Expected => transactions::TransactionStatus::Expected,
    }
}

fn income_kind_to_domain(e: transactions::IncomeKind) -> IncomeKind {
    match e {
        transactions::IncomeKind::Budgeted => IncomeKind::Budgeted,
        transactions::IncomeKind::New => IncomeKind::New,
    }
}

fn income_kind_to_entity(d: IncomeKind) -> transactions::IncomeKind {
    match d {
        IncomeKind::Budgeted => transactions::IncomeKind::Budgeted,
        IncomeKind::New => transactions::IncomeKind::New,
    }
}

// ---------------------------------------------------------------------------
// Shared inner builder (reduces duplication between the two `model_to_domain` variants)
// ---------------------------------------------------------------------------

fn build_transaction(m: transactions::Model, amount: Money) -> Transaction {
    Transaction {
        id: TransactionId::new(m.id),
        user_id: UserId::new(m.user_id),
        month_id: MonthId::new(m.month_id),
        category_id: m.category_id.map(CategoryId::new),
        account_id: m.account_id.map(AccountId::new),
        date: m.date,
        amount,
        description: m.description,
        source: source_to_domain(m.source),
        plaid_transaction_id: m.plaid_transaction_id,
        status: status_to_domain(m.status),
        income_kind: m.income_kind.map(income_kind_to_domain),
        is_rollover: m.is_rollover,
        is_fund_draw: m.is_fund_draw,
        matched_transaction_id: m.matched_transaction_id.map(TransactionId::new),
        created_at: m.created_at.with_timezone(&Utc),
        updated_at: m.updated_at.with_timezone(&Utc),
    }
}

// ---------------------------------------------------------------------------
// The single Plaid sign-flip site (BUDGET-PLAID-SIGN-1)
// ---------------------------------------------------------------------------

/// Flip a raw Plaid amount into the internal signed convention, exactly once
/// (`BUDGET-PLAID-SIGN-1`).
///
/// Plaid reports `amount > 0` for outflows (debits) and `amount < 0` for inflows
/// (credits/refunds). The internal convention is negative = expense, positive =
/// inflow, so the flip is a single negation. This is the ONE place the Plaid sign
/// is interpreted; both [`plaid_model_to_domain`] and [`plaid_dto_to_domain`]
/// route through it, and no downstream code re-interprets the Plaid sign.
///
/// The `debug_assert!` keys off `plaid_raw`'s sign as an INDEPENDENT signal: if
/// Plaid ever reverses their convention the raw value's polarity changes, so the
/// guard fires even though the negation stays mathematically correct. Zero rows
/// (some pending) are exempt.
fn flip_plaid_amount(plaid_raw: rust_decimal::Decimal) -> Money {
    let internal_amount = Money::from_decimal(-plaid_raw);
    debug_assert!(
        plaid_raw.is_zero()
            || (plaid_raw.is_sign_positive() && internal_amount.as_decimal().is_sign_negative())
            || (plaid_raw.is_sign_negative() && internal_amount.as_decimal().is_sign_positive()),
        "BUDGET-PLAID-SIGN-1 direction guard failed: plaid_raw={plaid_raw}, internal={:?}",
        internal_amount.as_decimal()
    );
    internal_amount
}

// ---------------------------------------------------------------------------
// Public mapper functions
// ---------------------------------------------------------------------------

/// Translate a stored `transactions` [`transactions::Model`] into a domain
/// [`Transaction`].
///
/// Trusts the stored `amount` sign (the internal convention: negative = expense,
/// positive = inflow). Use this when reading rows back from the DB.
///
/// Total — no validated newtypes on `Transaction`.
///
/// # Errors
///
/// Currently infallible; returns `Result` for a uniform mapper signature
/// (`MAPPER-1`) so every read-path entry point composes identically once
/// fallible aggregates are added.
pub fn model_to_domain(m: transactions::Model) -> Result<Transaction, MapperError> {
    let amount = Money::from_decimal(m.amount);
    Ok(build_transaction(m, amount))
}

/// Translate a freshly-fetched Plaid transaction row into a domain [`Transaction`],
/// normalizing the Plaid sign convention (`BUDGET-PLAID-SIGN-1`).
///
/// Plaid reports `amount > 0` for outflows (debits) and `amount < 0` for inflows
/// (credits/refunds). This function flips the sign once so that:
///   - debit  (Plaid positive → `amount > 0`) becomes expense (internal negative → `amount < 0`)
///   - credit (Plaid negative → `amount < 0`) becomes inflow (internal positive → `amount > 0`)
///
/// **Sign invariant (BUDGET-PLAID-SIGN-1):** after the flip, a nonzero Plaid debit
/// (`plaid_raw > 0`) must produce a negative internal amount, and a nonzero Plaid
/// credit (`plaid_raw < 0`) must produce a positive internal amount.  The
/// `debug_assert!` below is keyed directly off `plaid_raw`'s sign — an independent
/// signal from the arithmetic — so it would catch a future Plaid API change where
/// the sign convention reverses.
///
/// **Only this function handles Plaid-sign normalization.** No downstream code
/// re-interprets the Plaid sign (`BUDGET-PLAID-SIGN-1`).
///
/// Total — the sign flip cannot fail.
///
/// # Errors
///
/// Currently infallible; returns `Result` for a uniform mapper signature
/// (`MAPPER-1`) so the Plaid-ingest path composes identically to the regular
/// read path once fallible aggregates are added.
pub fn plaid_model_to_domain(m: transactions::Model) -> Result<Transaction, MapperError> {
    // `BUDGET-PLAID-SIGN-1`: Plaid positive-outflow → internal negative-expense.
    // The single flip site is `flip_plaid_amount`.
    let internal_amount = flip_plaid_amount(m.amount);
    Ok(build_transaction(m, internal_amount))
}

/// Translate a raw Plaid [`PlaidTransaction`] DTO directly into a domain
/// [`Transaction`], normalizing the Plaid sign once (`BUDGET-PLAID-SIGN-1`).
///
/// This is the Plaid-ingest mapper boundary used by the sync engine (`SPEC §6`):
/// the DTO carries Plaid's native data (positive = outflow) and this function
/// produces a domain `Transaction` with the internal sign and the supplied
/// context (the resolved `month_id`, optional `account_id`, `id`, `status`, and
/// timestamps). Categorization is deferred: a freshly-`added` row lands
/// `category_id = None` (uncategorized), so this function never sets a category.
///
/// `status` is decided by the caller from Plaid's `pending` flag (`SPEC §4.4`:
/// pending excluded, settled included) and passed in, because the pending->settled
/// transition is driven by the sync engine, not the mapper.
///
/// # Errors
/// Currently infallible (the flip cannot fail); returns `Result` for the uniform
/// mapper signature (`MAPPER-1`).
#[allow(clippy::too_many_arguments)]
pub fn plaid_dto_to_domain(
    dto: &PlaidTransaction,
    id: TransactionId,
    user_id: UserId,
    month_id: MonthId,
    account_id: Option<AccountId>,
    status: TransactionStatus,
    now: DateTime<Utc>,
) -> Result<Transaction, MapperError> {
    let amount = flip_plaid_amount(dto.amount);
    Ok(Transaction {
        id,
        user_id,
        month_id,
        // Plaid rows land uncategorized; the user assigns later (SPEC §6/§7).
        category_id: None,
        account_id,
        date: dto.date,
        amount,
        description: dto.name.clone(),
        source: TransactionSource::Plaid,
        plaid_transaction_id: Some(dto.transaction_id.clone()),
        status,
        income_kind: None,
        is_rollover: false,
        is_fund_draw: false,
        // A freshly-ingested Plaid row is never a matched placeholder.
        matched_transaction_id: None,
        created_at: now,
        updated_at: now,
    })
}

/// Translate a domain [`Transaction`] into a `transactions` [`transactions::ActiveModel`].
///
/// The `amount` stored is the internal signed value (negative = expense). No sign
/// flip — the flip is only at the Plaid ingest boundary (`plaid_model_to_domain`).
#[must_use]
pub fn domain_to_active_model(v: &Transaction) -> transactions::ActiveModel {
    transactions::ActiveModel {
        id: Set(v.id.value()),
        user_id: Set(v.user_id.value()),
        month_id: Set(v.month_id.value()),
        category_id: Set(v.category_id.map(|id| id.value())),
        account_id: Set(v.account_id.map(|id| id.value())),
        date: Set(v.date),
        amount: Set(v.amount.as_decimal()),
        description: Set(v.description.clone()),
        source: Set(source_to_entity(v.source)),
        plaid_transaction_id: Set(v.plaid_transaction_id.clone()),
        status: Set(status_to_entity(v.status)),
        income_kind: Set(v.income_kind.map(income_kind_to_entity)),
        is_rollover: Set(v.is_rollover),
        is_fund_draw: Set(v.is_fund_draw),
        matched_transaction_id: Set(v.matched_transaction_id.map(|id| id.value())),
        created_at: Set(v.created_at.into()),
        updated_at: Set(v.updated_at.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{NaiveDate, TimeZone};
    use rust_decimal::Decimal;
    use uuid::Uuid;

    fn sample_model(amount: Decimal) -> transactions::Model {
        let now = Utc.with_ymd_and_hms(2026, 6, 5, 12, 0, 0).unwrap();
        transactions::Model {
            id: Uuid::new_v4(),
            user_id: Uuid::new_v4(),
            month_id: Uuid::new_v4(),
            category_id: None,
            account_id: None,
            date: NaiveDate::from_ymd_opt(2026, 6, 5).unwrap_or(NaiveDate::MIN),
            amount,
            description: "Chipotle".to_owned(),
            source: transactions::TransactionSource::Plaid,
            plaid_transaction_id: Some("plaid-txn-123".to_owned()),
            status: transactions::TransactionStatus::Settled,
            income_kind: None,
            is_rollover: false,
            is_fund_draw: false,
            matched_transaction_id: None,
            created_at: now.into(),
            updated_at: now.into(),
        }
    }

    #[test]
    fn stored_negative_amount_is_expense() {
        // Stored rows already carry the internal sign (negative = expense).
        let m = sample_model(Decimal::new(-1250, 2)); // -$12.50
        let domain = model_to_domain(m).unwrap_or_else(|_| unreachable!());
        assert!(domain.amount.is_negative(), "expense should be negative");
        assert_eq!(domain.amount.as_decimal(), Decimal::new(-1250, 2));
    }

    #[test]
    fn plaid_positive_becomes_negative_expense() {
        // Plaid sends `12.50` for a $12.50 debit; we should store `-12.50`.
        let m = sample_model(Decimal::new(1250, 2)); // Plaid positive = outflow
        let domain = plaid_model_to_domain(m).unwrap_or_else(|_| unreachable!());
        assert!(
            domain.amount.is_negative(),
            "Plaid debit should map to negative expense"
        );
        assert_eq!(domain.amount.as_decimal(), Decimal::new(-1250, 2));
    }

    #[test]
    fn plaid_negative_becomes_positive_inflow() {
        // Plaid sends `-50.00` for a $50 refund/credit; we should store `+50.00`.
        let m = sample_model(Decimal::new(-5000, 2)); // Plaid negative = inflow
        let domain = plaid_model_to_domain(m).unwrap_or_else(|_| unreachable!());
        assert!(
            domain.amount.is_positive(),
            "Plaid credit should map to positive inflow"
        );
        assert_eq!(domain.amount.as_decimal(), Decimal::new(5000, 2));
    }

    #[test]
    fn zero_amount_passes_direction_test() {
        let m = sample_model(Decimal::ZERO);
        let domain = plaid_model_to_domain(m).unwrap_or_else(|_| unreachable!());
        assert!(domain.amount.is_zero());
    }

    #[test]
    fn rollover_transaction_flag_preserved() {
        let mut m = sample_model(Decimal::new(21200, 2)); // +$212.00 rollover
        m.is_rollover = true;
        m.source = transactions::TransactionSource::Manual;
        let domain = model_to_domain(m).unwrap_or_else(|_| unreachable!());
        assert!(domain.is_rollover);
    }

    #[test]
    fn fund_draw_flag_round_trips_both_directions() {
        // D6 Model A: the is_fund_draw flag (surplus draw / sinking payout) must
        // survive read AND write so the netting exclusion is durable.
        let mut m = sample_model(Decimal::new(-80000, 2)); // -$800 surplus draw
        m.is_fund_draw = true;
        m.source = transactions::TransactionSource::Manual;
        let domain = model_to_domain(m).unwrap_or_else(|_| unreachable!());
        assert!(domain.is_fund_draw, "is_fund_draw preserved on read");

        let active = domain_to_active_model(&domain);
        assert_eq!(
            active.is_fund_draw,
            sea_orm::ActiveValue::Set(true),
            "is_fund_draw preserved on write"
        );
    }

    #[test]
    fn matched_transaction_id_round_trips_both_directions() {
        // BUDGET-SETTLE-ON-MATCH-1: the match link must survive read AND write so
        // the placeholder/real pair stays linked durably.
        let real_id = Uuid::new_v4();
        let mut m = sample_model(Decimal::new(-80000, 2)); // -$800 placeholder
        m.status = transactions::TransactionStatus::Expected;
        m.source = transactions::TransactionSource::Manual;
        m.matched_transaction_id = Some(real_id);
        let domain = model_to_domain(m).unwrap_or_else(|_| unreachable!());
        assert_eq!(
            domain.matched_transaction_id,
            Some(TransactionId::new(real_id)),
            "matched_transaction_id preserved on read"
        );
        assert!(
            domain.is_matched_placeholder(),
            "an expected row with a link is a matched placeholder"
        );

        let active = domain_to_active_model(&domain);
        assert_eq!(
            active.matched_transaction_id,
            sea_orm::ActiveValue::Set(Some(real_id)),
            "matched_transaction_id preserved on write"
        );
    }

    #[test]
    fn unmatched_rows_carry_no_link() {
        // A settled/plaid row never carries a match link.
        let m = sample_model(Decimal::new(-1250, 2));
        let domain = model_to_domain(m).unwrap_or_else(|_| unreachable!());
        assert_eq!(domain.matched_transaction_id, None);
        assert!(!domain.is_matched_placeholder());
    }

    #[test]
    fn all_statuses_map() {
        for (entity_status, expected) in [
            (
                transactions::TransactionStatus::Pending,
                TransactionStatus::Pending,
            ),
            (
                transactions::TransactionStatus::Settled,
                TransactionStatus::Settled,
            ),
            (
                transactions::TransactionStatus::Expected,
                TransactionStatus::Expected,
            ),
        ] {
            let mut m = sample_model(Decimal::new(-1000, 2));
            m.status = entity_status;
            let domain = model_to_domain(m).unwrap_or_else(|_| unreachable!());
            assert_eq!(domain.status, expected);
        }
    }

    #[test]
    fn income_kind_maps_when_present() {
        let mut m = sample_model(Decimal::new(500_000, 2)); // +$5000 paycheck
        m.income_kind = Some(transactions::IncomeKind::Budgeted);
        m.source = transactions::TransactionSource::Manual;
        let domain = model_to_domain(m).unwrap_or_else(|_| unreachable!());
        assert_eq!(domain.income_kind, Some(IncomeKind::Budgeted));
        assert!(domain.is_income());
    }

    #[test]
    fn active_model_preserves_amount_sign() {
        let m = sample_model(Decimal::new(-1250, 2));
        let domain = model_to_domain(m).unwrap_or_else(|_| unreachable!());
        let am = domain_to_active_model(&domain);
        assert_eq!(am.amount, Set(Decimal::new(-1250, 2)));
    }
}
