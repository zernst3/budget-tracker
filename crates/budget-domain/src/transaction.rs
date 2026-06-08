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
}
