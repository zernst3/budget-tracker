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

use crate::category::Category;
use crate::enums::{CategoryGrp, TransactionStatus};
use crate::ids::{CategoryId, TransactionId};
use crate::money::Money;
use crate::transaction::Transaction;

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

/// `BUDGET-FUND-EARMARK-1` + `BUDGET-NO-DOUBLE-CHARGE-1` (D6 Model A / D7) — the
/// single predicate deciding whether a transaction contributes to a month's
/// expense remaining sum (the `Σ(expense category remaining)` term of the `D5`
/// net, `SPEC §4.3`).
///
/// A transaction's signed `amount` flows into the month-expense sum iff ALL of:
///   - it counts in budget by status ([`counts_in_budget`],
///     `BUDGET-STATUS-DRIVES-INCLUSION-1`),
///   - it is **not** income (income belongs to the `(actual - expected)` term,
///     `D5`),
///   - it is **not** a fund **draw** (`is_fund_draw = true`: surplus draw, sinking
///     payout). Under D6 Model A the money was already expensed at CONTRIBUTION
///     time, so the later draw is a fund-draw, not a re-charged budget expense
///     (`BUDGET-NO-DOUBLE-CHARGE-1`); excluding it here keeps the dollar counted
///     exactly once, and
///   - it is **not** a buffer-financed full-price purchase
///     (`buffer_financed_txn_ids`). That row posts for TRACKING only with ZERO
///     month-budget impact: the buffer draw fronts the cash, and the *budget*
///     effect is the compulsory installments (ordinary expenses) flowing back into
///     the buffer (`SPEC §4.9` D7). Excluding the full price here is exactly what
///     stops it from blowing up its month while the installments are counted.
///
/// **D6 Model A (`BUDGET-FUND-EARMARK-1`).** A fund CONTRIBUTION — sinking accrual,
/// surplus contribution, buffer-repayment installment — is a manual Other-bucket
/// expense that **COUNTS** in the net, reducing the rolling Other by the
/// contribution while the fund balance rises by the same amount. Contributions are
/// therefore NOT excluded here (they are ordinary `-amount` expenses with
/// `is_fund_draw = false`): the earmark bites exactly once, through that Other
/// expense, and fund balances are never separately subtracted from free-to-spend.
/// `fund_category_ids` is retained on the signature for the buffer-financed /
/// month-lifecycle plumbing but no longer drives a contribution exclusion (that was
/// the rejected Model-B total-exclusion behaviour).
///
/// This is the one place the rolling-Other expense exclusions live, so the
/// month-lifecycle netting (build step 4) and the fund service (build step 5)
/// cannot drift apart. Rollover rows (`is_rollover = true`) are *not* excluded:
/// they are a real signed line item in Other, the auditable carryover
/// (`BUDGET-ROLLOVER-INTEGRITY-1`).
#[must_use]
pub fn counts_in_month_expense_remaining(
    txn: &Transaction,
    fund_category_ids: &[CategoryId],
    buffer_financed_txn_ids: &[TransactionId],
) -> bool {
    // `fund_category_ids` is intentionally unused for the inclusion decision under
    // D6 Model A (contributions now COUNT); it is kept on the signature so the
    // month-lifecycle netting call site stays stable while the buffer-financed
    // exclusion plumbing carries forward.
    let _ = fund_category_ids;
    counts_in_budget(txn.status)
        // A matched expected placeholder no longer reserves budget — the real
        // transaction it links to counts instead, so the pair counts exactly once
        // (BUDGET-SETTLE-ON-MATCH-1 / BUDGET-NO-DOUBLE-CHARGE-1).
        && !txn.is_matched_placeholder()
        && !txn.is_income()
        && !txn.is_fund_draw
        && !buffer_financed_txn_ids.contains(&txn.id)
}

/// `BUDGET-NO-DOUBLE-CHARGE-1` / `SPEC §4.5` — the per-category envelope spent
/// for the month, given the signed sum of that category's budget-counting
/// transactions.
///
/// This is the single place the envelope-summary (`SPEC §7`) decides a
/// category's displayed spent. It lifts the two-line classification out of the
/// server function so the rule is a tested domain fact, not inline UI logic:
///
///   - A **fixed** category (`CategoryGrp::Fixed`) uses
///     [`fixed_category_spent`]: while UNSETTLED its spent is the budgeted
///     placeholder (`-amount`, an outflow); once SETTLED its spent is the real
///     transaction sum. Settlement is proxied here by "has at least one
///     budget-counting transaction" (`counting_sum != ZERO`), because the SQL
///     aggregate that produces `counting_sum` already applied the
///     `BUDGET-STATUS-DRIVES-INCLUSION-1` filter, so a non-zero sum means a real
///     counting charge has landed. (A genuine $0.00 settled charge is
///     indistinguishable from "no charge" at this granularity and stays on the
///     placeholder — the conservative read-only choice; a manual "mark settled"
///     button, `SPEC §4.2`, is the precise override and is not part of this read.)
///   - A **discretionary** category uses the raw transaction sum (no
///     placeholder; you only ever spent what you spent).
///
/// `counting_sum` is the signed sum (negative = outflow) of the category's
/// settled + expected transactions in the month (the
/// [`crate::projections::CategorySpent::spent`] value); the rollover bucket's
/// `counting_sum` is the rolling-Other balance and is returned verbatim (it is a
/// discretionary-style raw sum, never a placeholder).
#[must_use]
pub fn envelope_category_spent(category: &Category, counting_sum: Money) -> Money {
    // The rollover bucket is never a fixed placeholder: its spent IS the rolling
    // Other balance (the signed sum of its line items), returned as-is.
    if category.is_rollover_bucket || category.grp == CategoryGrp::Discretionary {
        return counting_sum;
    }
    // Fixed category: settled ? sum : placeholder (BUDGET-NO-DOUBLE-CHARGE-1).
    let settlement = if counting_sum == Money::ZERO {
        FixedSettlement::Unsettled
    } else {
        FixedSettlement::Settled
    };
    // Placeholder = the budgeted amount as an outflow (negative).
    let placeholder = -category.amount;
    fixed_category_spent(settlement, placeholder, counting_sum)
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

    // -- D6 Model A: counts_in_month_expense_remaining ---------------------

    /// A bare expense transaction for the Model-A inclusion tests.
    fn expense(amount: Money, category_id: Option<CategoryId>, is_fund_draw: bool) -> Transaction {
        use chrono::{NaiveDate, Utc};

        use crate::enums::TransactionSource;
        use crate::ids::{MonthId, TransactionId, UserId};

        let now = Utc::now();
        Transaction {
            id: TransactionId::generate(),
            user_id: UserId::generate(),
            month_id: MonthId::generate(),
            category_id,
            account_id: None,
            date: NaiveDate::from_ymd_opt(2026, 6, 8).unwrap_or(NaiveDate::MIN),
            amount,
            description: "t".to_owned(),
            source: TransactionSource::Manual,
            plaid_transaction_id: None,
            status: TransactionStatus::Settled,
            income_kind: None,
            is_rollover: false,
            is_fund_draw,
            matched_transaction_id: None,
            comment: None,
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn d6_model_a_fund_contribution_on_fund_category_counts() {
        // D6 Model A: a contribution (is_fund_draw=false) on a fund-bound category
        // COUNTS — the fund category id no longer drives an exclusion.
        let fund_cat = CategoryId::generate();
        let contribution = expense(Money::from_minor(-5_000), Some(fund_cat), false);
        assert!(
            counts_in_month_expense_remaining(&contribution, &[fund_cat], &[]),
            "a fund contribution must COUNT in the month expense sum (D6 Model A)"
        );
    }

    #[test]
    fn d6_model_a_fund_draw_is_excluded() {
        // A fund DRAW (is_fund_draw=true) is excluded — money already expensed at
        // contribution time (BUDGET-NO-DOUBLE-CHARGE-1).
        let fund_cat = CategoryId::generate();
        let draw = expense(Money::from_minor(-80_000), Some(fund_cat), true);
        assert!(
            !counts_in_month_expense_remaining(&draw, &[fund_cat], &[]),
            "a fund draw must be excluded from the month expense sum"
        );
    }

    #[test]
    fn d6_model_a_buffer_financed_full_price_excluded_via_list() {
        // The buffer-financed full-price tracking row is excluded via its
        // obligation-keyed list, NOT the fund-draw flag (D7).
        let full_price = expense(Money::from_minor(-120_000), None, false);
        let ids = [full_price.id];
        assert!(
            !counts_in_month_expense_remaining(&full_price, &[], &ids),
            "buffer-financed full price must be excluded via its txn-id list"
        );
    }

    #[test]
    fn d6_model_a_ordinary_expense_counts() {
        let cat = CategoryId::generate();
        let e = expense(Money::from_minor(-2_500), Some(cat), false);
        assert!(counts_in_month_expense_remaining(&e, &[], &[]));
    }

    #[test]
    fn matched_expected_placeholder_is_excluded() {
        // BUDGET-SETTLE-ON-MATCH-1: an expected placeholder that has been matched
        // to a real txn no longer counts (the real txn counts instead), so the
        // pair counts exactly once.
        let cat = CategoryId::generate();
        let mut placeholder = expense(Money::from_minor(-80_000), Some(cat), false);
        placeholder.status = TransactionStatus::Expected;
        // Unmatched: an expected placeholder reserves budget -> counts.
        assert!(
            counts_in_month_expense_remaining(&placeholder, &[], &[]),
            "an unmatched expected placeholder reserves budget"
        );
        // Matched: drops out.
        placeholder.matched_transaction_id = Some(crate::ids::TransactionId::generate());
        assert!(placeholder.is_matched_placeholder());
        assert!(
            !counts_in_month_expense_remaining(&placeholder, &[], &[]),
            "a matched expected placeholder is excluded (BUDGET-SETTLE-ON-MATCH-1)"
        );
    }

    // -- envelope_category_spent (SPEC §4.5 / §7) --------------------------

    /// A category fixture for the envelope tests.
    fn category(grp: CategoryGrp, amount: Money, is_rollover_bucket: bool) -> Category {
        use crate::enums::Cadence;
        use crate::ids::{BudgetId, CategoryId, CategoryKey};

        Category {
            id: CategoryId::generate(),
            budget_id: BudgetId::generate(),
            category_key: CategoryKey::generate(),
            name: "cat".to_owned(),
            amount,
            grp,
            settle_type: None,
            expected_bills: None,
            is_rollover_bucket,
            cadence: Cadence::Monthly,
            period_months: None,
            fund_balance: Money::ZERO,
            next_due_date: None,
            sort_order: 0,
        }
    }

    #[test]
    fn envelope_fixed_unsettled_uses_placeholder_not_sum() {
        // No counting transactions yet -> spent == the budgeted placeholder
        // (-amount), NEVER the placeholder plus any stray sum.
        let cat = category(CategoryGrp::Fixed, Money::from_minor(200_000), false); // $2000 rent
        let spent = envelope_category_spent(&cat, Money::ZERO);
        assert_eq!(spent, Money::from_minor(-200_000));
    }

    #[test]
    fn envelope_fixed_settled_uses_real_sum_not_placeholder_plus_sum() {
        // A real charge landed (non-zero counting sum) -> spent is the real sum,
        // and is provably NOT placeholder + sum (the double-charge bug).
        let cat = category(CategoryGrp::Fixed, Money::from_minor(200_000), false);
        let real = Money::from_minor(-201_500); // $2015 actual rent
        let spent = envelope_category_spent(&cat, real);
        assert_eq!(spent, real);
        assert_ne!(spent, Money::from_minor(-200_000) + real);
    }

    #[test]
    fn envelope_discretionary_is_the_raw_sum() {
        // Discretionary never uses a placeholder: spent is exactly what was spent.
        let cat = category(CategoryGrp::Discretionary, Money::from_minor(50_000), false);
        let sum = Money::from_minor(-31_277);
        assert_eq!(envelope_category_spent(&cat, sum), sum);
        // Zero spend on a discretionary category is zero, not the budget.
        assert_eq!(envelope_category_spent(&cat, Money::ZERO), Money::ZERO);
    }

    #[test]
    fn envelope_rollover_bucket_returns_balance_verbatim_even_if_fixed_grp() {
        // The rollover bucket's spent IS the rolling-Other balance — never a
        // placeholder, even if its grp were Fixed.
        let cat = category(CategoryGrp::Fixed, Money::from_minor(0), true);
        let other_balance = Money::from_minor(21_200); // +$212 carryover
        assert_eq!(envelope_category_spent(&cat, other_balance), other_balance);
    }
}
