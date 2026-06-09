//! The [`Transaction`] aggregate — the central record type (`SPEC §5`).
//!
//! Covers regular expenses/income (Plaid or manual), the system-generated
//! rollover line item (`is_rollover = true`, `BUDGET-ROLLOVER-INTEGRITY-1`),
//! expected-expense placeholders (`status = Expected`, `SPEC §4.10`), and income
//! flows (`income_kind` set, `SPEC §4.8`).
//!
//! [`Transaction::amount`] is signed: **negative = expense, positive = inflow**
//! (the internal convention). Plaid amounts are flipped once at the mapper
//! boundary (`BUDGET-PLAID-SIGN-1`); no domain code re-interprets Plaid sign.
//! Amount uses [`Money`] (`BUDGET-MONEY-1`).
//!
//! Whether a transaction counts toward budget math is decided by
//! [`crate::predicates::counts_in_budget`] keyed on [`Transaction::status`]
//! (`BUDGET-STATUS-DRIVES-INCLUSION-1`) — never by an inline status match here.

use chrono::{DateTime, NaiveDate, Utc};

use crate::enums::{IncomeKind, TransactionSource, TransactionStatus};
use crate::ids::{AccountId, CategoryId, MonthId, TransactionId, UserId};
use crate::money::Money;
use crate::predicates::counts_in_budget;

/// A transaction record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transaction {
    /// Stable identity.
    pub id: TransactionId,
    /// Owning user.
    pub user_id: UserId,
    /// The month this transaction belongs to (always non-null; expected expenses
    /// targeting a future month eager-create that month, `SPEC §4.10`).
    pub month_id: MonthId,
    /// Assigned category; `None` = uncategorized (freshly pulled, awaiting assignment).
    pub category_id: Option<CategoryId>,
    /// Linked account; `None` for a manual entry without an account.
    pub account_id: Option<AccountId>,
    /// Actual purchase / post date.
    pub date: NaiveDate,
    /// Signed amount: negative = expense, positive = inflow (`BUDGET-PLAID-SIGN-1`).
    pub amount: Money,
    /// Description / payee. Free-form, no validation.
    pub description: String,
    /// Whether this came from Plaid or was entered manually.
    pub source: TransactionSource,
    /// Plaid stable transaction id for dedup; `None` for manual rows. UNIQUE at
    /// the DB level when present (`ENTITIES-8`).
    pub plaid_transaction_id: Option<String>,
    /// Settlement / inclusion status (drives [`Transaction::counts_in_budget`]).
    pub status: TransactionStatus,
    /// Income sub-kind for income-flow rows; `None` for expense/rollover rows.
    pub income_kind: Option<IncomeKind>,
    /// `true` for the system-generated 1st-of-month rollover line item
    /// (`BUDGET-ROLLOVER-INTEGRITY-1`). A DB partial unique on `(month_id) WHERE
    /// is_rollover` prevents double-posting.
    pub is_rollover: bool,
    /// `true` for a fund DRAW that must NOT be re-charged against the month budget
    /// (surplus draw, sinking payout; `BUDGET-NO-DOUBLE-CHARGE-1` / D6 Model A).
    ///
    /// Under D6 Model A the money was already expensed at CONTRIBUTION time (the
    /// contribution counts in the net); the later draw is a fund-draw, not a
    /// re-charged budget expense, so it is excluded from the month
    /// expense-remaining sum
    /// ([`crate::predicates::counts_in_month_expense_remaining`]). Contributions,
    /// installments, and sinking accruals are NOT draws — they leave this `false`
    /// and therefore COUNT. The buffer-financed full-price tracking row uses its own
    /// obligation-keyed exclusion (`SPEC §4.9` D7) and leaves this `false`.
    pub is_fund_draw: bool,
    /// The real transaction that settled this `expected` placeholder
    /// (`SPEC §4.10` / `§12`, `BUDGET-SETTLE-ON-MATCH-1`). `Some` ONLY on an
    /// `expected` placeholder that has been matched to a real charge; `None`
    /// otherwise.
    ///
    /// While this is `Some`, the placeholder is **matched** and is excluded from
    /// budget math (the real transaction counts instead), so the pair counts
    /// exactly once (`BUDGET-NO-DOUBLE-CHARGE-1`). The reverse path (Plaid
    /// `removed`) reads this to find and restore the placeholder, then clears it
    /// back to `None`.
    pub matched_transaction_id: Option<TransactionId>,
    /// When the row was created (UTC, `DOMAIN-7`).
    pub created_at: DateTime<Utc>,
    /// When the row was last updated (UTC, `DOMAIN-7`).
    pub updated_at: DateTime<Utc>,
}

impl Transaction {
    /// Whether this transaction counts toward budget math, via the single shared
    /// inclusion predicate (`BUDGET-STATUS-DRIVES-INCLUSION-1`). This is a thin
    /// convenience over [`counts_in_budget`]; it does NOT re-implement the rule.
    #[must_use]
    pub const fn counts_in_budget(&self) -> bool {
        counts_in_budget(self.status)
    }

    /// `true` when this row represents an income inflow (`income_kind` is set).
    #[must_use]
    pub const fn is_income(&self) -> bool {
        self.income_kind.is_some()
    }

    /// `true` when this is an `expected` placeholder that has been matched to a
    /// real transaction (`BUDGET-SETTLE-ON-MATCH-1`). A matched placeholder no
    /// longer reserves budget — the real transaction it links to counts instead,
    /// so the pair counts exactly once (`BUDGET-NO-DOUBLE-CHARGE-1`).
    ///
    /// This is keyed on the link, not on status: only an `expected` row ever
    /// carries `matched_transaction_id`, but checking the status as well keeps the
    /// predicate honest if a non-placeholder row is ever mislabeled.
    #[must_use]
    pub const fn is_matched_placeholder(&self) -> bool {
        matches!(self.status, TransactionStatus::Expected) && self.matched_transaction_id.is_some()
    }
}
