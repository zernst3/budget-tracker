//! Typed domain enums mirroring the Postgres `pgEnum` columns.
//!
//! These are the framework-free domain counterparts of the `SeaORM`
//! `DeriveActiveEnum` types in `budget-entities` (`ENTITIES-12`). The mappers
//! crate translates between the two 1:1. Keeping a separate domain copy keeps
//! the domain crate free of any `SeaORM` dependency (`DOMAIN-1`) while still
//! giving business logic compile-time exhaustive `match` over the same variants.
//!
//! Each enum carries `serde` derives so it threads through DTOs, plus a
//! [`Default`] only where the schema declares a column default.

use serde::{Deserialize, Serialize};

/// Bucket group: predictable fixed expenses vs. discretionary spending
/// (`SPEC §4.2`; entity enum `category_grp`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CategoryGrp {
    /// Predictable, fixed expense (rent, phone, utilities).
    Fixed,
    /// Discretionary spending (groceries, fun, etc.).
    Discretionary,
}

/// Settle type for fixed categories (`SPEC §4.2`; entity enum `settle_type`).
///
/// `None` of this enum (i.e. `Option::None` on the field) means a discretionary
/// category, which has no settle semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SettleType {
    /// Amount known in advance and stable (rent, phone). Settled at month start;
    /// overspend reconciles to Other like everything else.
    TrueSet,
    /// Budget is a placeholder until the real bill(s) land (utilities). Carries
    /// a `pending` -> `settled` lifecycle once `expected_bills` transactions arrive.
    FlexibleSet,
}

/// Accrual cadence (`SPEC §4.7`; entity enum `cadence`). Anything longer than
/// monthly makes the category a sinking fund.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Cadence {
    /// Normal reconciled monthly category (the schema default).
    #[default]
    Monthly,
    /// Every 3 months.
    Quarterly,
    /// Every 6 months.
    Semiannual,
    /// Once a year.
    Annual,
}

impl Cadence {
    /// The implied period in months, used as the sinking-fund accrual divisor
    /// when `period_months` is not explicitly overridden (`SPEC §4.7`).
    #[must_use]
    pub const fn period_months(self) -> u32 {
        match self {
            Cadence::Monthly => 1,
            Cadence::Quarterly => 3,
            Cadence::Semiannual => 6,
            Cadence::Annual => 12,
        }
    }

    /// `true` when this cadence makes the category a sinking fund (`> monthly`).
    #[must_use]
    pub const fn is_sinking_fund(self) -> bool {
        !matches!(self, Cadence::Monthly)
    }
}

/// Account type (`SPEC §5`; entity enum `account_type`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountType {
    /// A checking account.
    Checking,
    /// A credit card account.
    Credit,
    /// A savings account.
    Savings,
    /// An investment account.
    Investment,
    /// Any other account type.
    Other,
}

/// Month lifecycle status (`SPEC §4.6`; entity enum `month_status`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MonthStatus {
    /// The month is open and accepting transactions.
    Open,
    /// The month is closed; its net has rolled forward into the next month.
    Closed,
}

/// Transaction source (`SPEC §5`; entity enum `transaction_source`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransactionSource {
    /// Entered by the user by hand.
    Manual,
    /// Pulled from Plaid.
    Plaid,
}

/// Settlement / inclusion status (`SPEC §4.4`, `§4.10`;
/// `BUDGET-STATUS-DRIVES-INCLUSION-1`; entity enum `transaction_status`).
///
/// The inclusion polarity (which statuses count toward budget math) is decided
/// in exactly one place — [`crate::predicates::counts_in_budget`] — not by
/// matching on this enum at each aggregation site.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransactionStatus {
    /// Plaid-seen but not yet settled — EXCLUDED from budget math (`SPEC §4.4`).
    Pending,
    /// A real, confirmed transaction — INCLUDED.
    Settled,
    /// A manual placeholder for a known future charge — INCLUDED; it reserves
    /// budget (`SPEC §4.10`).
    Expected,
}

/// Income sub-kind for income-flow transactions (`SPEC §4.8`;
/// entity enum `income_kind`). `None` on the field means an expense / rollover row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IncomeKind {
    /// Recurring paycheck — reconciles against the month's expected income.
    Budgeted,
    /// Unplanned inflow (gift, refund, bonus, side gig) — pure addition.
    New,
}

/// Fund kind (`SPEC §4.9`; entity enum `fund_kind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FundKind {
    /// Emergency / working savings pool; `compulsory_repayment = true`. Drawing
    /// creates a [`crate::repayment_obligation::RepaymentObligation`].
    Buffer,
    /// Deliberate surplus saved toward a planned purchase; `compulsory_repayment = false`.
    Surplus,
}

/// Repayment obligation lifecycle status (`SPEC §5`; entity enum `obligation_status`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObligationStatus {
    /// Still being repaid (`remaining_amount > 0`).
    Active,
    /// Fully repaid (`remaining_amount == 0`).
    Paid,
}

/// How expected monthly income is computed (`SPEC §4.8`; entity enum `income_mode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IncomeMode {
    /// Exact: expected = paychecks-this-month × amount. No buffer (the default).
    #[default]
    PerPaycheck,
    /// Averaged: expected = (amount × `paychecks_per_year`) ÷ 12. Needs a buffer.
    Smoothed,
}

/// Paycheck cadence (`SPEC §4.8`; entity enum `paycheck_type`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaycheckType {
    /// 24/yr — always exactly 2 paychecks per month (Zach's cadence).
    Semimonthly,
    /// 26/yr — 2–3 paychecks per month.
    Biweekly,
    /// 52/yr — 4–5 paychecks per month.
    Weekly,
    /// Variable / hourly — `amount` is `None`; degrades to pure actual-tracking.
    Hourly,
}

impl PaycheckType {
    /// Paychecks per year for the fixed cadences. `Hourly` is variable, so this
    /// returns `None` (no annual count can be assumed).
    #[must_use]
    pub const fn paychecks_per_year(self) -> Option<u32> {
        match self {
            PaycheckType::Semimonthly => Some(24),
            PaycheckType::Biweekly => Some(26),
            PaycheckType::Weekly => Some(52),
            PaycheckType::Hourly => None,
        }
    }
}

/// Default routing for over-expected income (`SPEC §4.8`; entity enum `surplus_routing`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SurplusRouting {
    /// Accumulate surplus in the income smoothing buffer (the default).
    #[default]
    Buffer,
    /// Add surplus to this month's free-to-spend (the Other bucket).
    ThisMonth,
    /// Route surplus to external savings (outside the app's tracking).
    Savings,
}
