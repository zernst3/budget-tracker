//! Pure budget predicates — the correctness invariants of the app, expressed as
//! free functions with no I/O.
//!
//! Two named rules live here as single source-of-truth functions so every
//! aggregation routes through them and they cannot drift:
//!   - [`counts_in_budget`] — `BUDGET-STATUS-DRIVES-INCLUSION-1`: which
//!     transaction statuses count toward budget math.
//!   - [`fixed_category_spent`] — `BUDGET-NO-DOUBLE-CHARGE-1`: a fixed
//!     category's spent is `settled ? sum(txns) : placeholder`, never both.
//!
//! Both are exhaustively unit-tested in this module. Keeping them here (not in a
//! service) means the rule is a compile-checked, test-covered domain fact rather
//! than an inline filter repeated per query.

use crate::enums::TransactionStatus;
use crate::money::Money;

/// `BUDGET-STATUS-DRIVES-INCLUSION-1` — the single inclusion predicate.
///
/// Whether a transaction counts toward budget math (category spent, month net,
/// free-to-spend) is decided here, keyed on status:
///   - [`TransactionStatus::Settled`] -> included,
///   - [`TransactionStatus::Expected`] -> included (it reserves budget, `SPEC §4.10`),
///   - [`TransactionStatus::Pending`] -> excluded (transient Plaid-seen, `SPEC §4.4`).
///
/// The deliberately-opposite handling of `pending` (excluded) and `expected`
/// (included) is encoded in this one place. Every aggregation MUST call this
/// rather than inlining a status filter.
#[must_use]
pub const fn counts_in_budget(status: TransactionStatus) -> bool {
    match status {
        TransactionStatus::Settled | TransactionStatus::Expected => true,
        TransactionStatus::Pending => false,
    }
}

/// The settlement state of a fixed category, for [`fixed_category_spent`].
///
/// A fixed category (`SPEC §4.2`) is either:
///   - settled — its real transactions have replaced the placeholder, OR
///   - unsettled — the budgeted placeholder still stands in for the not-yet-arrived bill.
///
/// For a `flexible_set` category this is "all `expected_bills` have arrived";
/// for a `true_set` category it is "at least one real transaction was assigned"
/// (the caller decides which, then passes the resolved boolean here).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FixedSettlement {
    /// Real transactions have replaced the placeholder; spent = their sum.
    Settled,
    /// No real transactions yet; spent = the budgeted placeholder.
    Unsettled,
}

/// `BUDGET-NO-DOUBLE-CHARGE-1` — the single fixed-category spent predicate.
///
/// Computes a fixed category's spent amount with exactly one rule:
///   - `Settled`   -> `sum_of_settled_transactions` (the placeholder is gone), else
///   - `Unsettled` -> `placeholder` (the budgeted amount stands in).
///
/// The two are NEVER added. This is the `SPEC §4.5` invariant and the root of
/// correctness for fixed expenses: assigning a real "rent" transaction to an
/// unsettled category settles it and REPLACES the placeholder rather than
/// stacking on top. The same match-and-replace semantics also govern
/// `flexible_set` settlement and expected-expense matching
/// (`BUDGET-SETTLE-ON-MATCH-1`), which resolve to this single function.
///
/// `placeholder` is conventionally a negative [`Money`] (an expense) and
/// `sum_of_settled_transactions` is the signed sum of the category's settled
/// rows; the function does not re-interpret sign — it only chooses which term to
/// return.
#[must_use]
pub fn fixed_category_spent(
    settlement: FixedSettlement,
    placeholder: Money,
    sum_of_settled_transactions: Money,
) -> Money {
    match settlement {
        FixedSettlement::Settled => sum_of_settled_transactions,
        FixedSettlement::Unsettled => placeholder,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inclusion_polarity_is_settled_yes_expected_yes_pending_no() {
        // BUDGET-STATUS-DRIVES-INCLUSION-1: enumerate all three statuses.
        assert!(counts_in_budget(TransactionStatus::Settled));
        assert!(counts_in_budget(TransactionStatus::Expected));
        assert!(!counts_in_budget(TransactionStatus::Pending));
    }

    #[test]
    fn unsettled_fixed_category_spent_is_exactly_the_placeholder() {
        // BUDGET-NO-DOUBLE-CHARGE-1: while unsettled, spent == placeholder.
        let placeholder = Money::from_minor(-200_000); // -$2,000 rent
        let spent = fixed_category_spent(
            FixedSettlement::Unsettled,
            placeholder,
            // Any stray sum must be IGNORED while unsettled.
            Money::from_minor(-150_000),
        );
        assert_eq!(spent, placeholder);
    }

    #[test]
    fn settled_fixed_category_spent_is_the_real_sum_not_placeholder_plus_sum() {
        // The core no-double-charge case: assign a real rent txn -> spent is the
        // real txn, NOT placeholder + txn.
        let placeholder = Money::from_minor(-200_000); // -$2,000 placeholder
        let real_txn = Money::from_minor(-201_500); // -$2,015 actual rent
        let spent = fixed_category_spent(FixedSettlement::Settled, placeholder, real_txn);
        assert_eq!(spent, real_txn);
        // Explicitly assert it is NOT the double-counted figure.
        assert_ne!(spent, placeholder + real_txn);
    }

    #[test]
    fn settled_with_multiple_bills_uses_their_sum() {
        // flexible_set utilities: electricity + gas both arrived -> sum of both.
        let placeholder = Money::from_minor(-15_000); // -$150 placeholder
        let electricity = Money::from_minor(-8_012);
        let gas = Money::from_minor(-6_433);
        let spent = fixed_category_spent(FixedSettlement::Settled, placeholder, electricity + gas);
        assert_eq!(spent, Money::from_minor(-8_012 - 6_433));
        assert_ne!(spent, placeholder);
    }
}
