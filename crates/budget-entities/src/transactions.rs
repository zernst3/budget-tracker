//! `SeaORM` entity for `transactions`.
//!
//! Schema source: `SPEC.md §5` — the authoritative definition for the budget tracker.
//!
//! Pattern: per ENTITIES-1..6, ENTITIES-8, ENTITIES-12 in `agora-rs/docs/CONVENTIONS.md`.
//!
//! Central record type. Covers:
//!   - Regular expense/income records (pulled from Plaid or entered manually)
//!   - Rollover system transactions (`is_rollover = true`, `BUDGET-ROLLOVER-INTEGRITY-1`)
//!   - Expected expense placeholders (`status = 'expected'`, SPEC §4.10)
//!   - Income flows (`income_kind` non-null, SPEC §4.8)
//!
//! `amount` is signed: **negative = expense, positive = inflow** (internal convention).
//! Plaid amounts are flipped once at the mapper boundary (`BUDGET-PLAID-SIGN-1`).
//! `amount` uses `Decimal` per `BUDGET-MONEY-1` / `DOMAIN-8` — never f64.
//!
//! Budget inclusion predicate (`BUDGET-STATUS-DRIVES-INCLUSION-1`):
//!   `settled` → included, `expected` → included (reserves budget), `pending` → excluded.
//!
//! `plaid_transaction_id` is UNIQUE (dedup). Per ENTITIES-8, declared at the DB level only.
//!
//! A DB partial unique index on `(month_id) WHERE is_rollover` prevents double-posting
//! the rollover (`BUDGET-ROLLOVER-INTEGRITY-1`). Per ENTITIES-8, declared at DB level only.
//!
//! Per ENTITIES-4 the parent-side `has_many` declarations for both `months` and
//! `categories` point to this entity.

use sea_orm::entity::prelude::*;

/// Transaction source (SPEC §5).
#[derive(Copy, Clone, Debug, PartialEq, Eq, EnumIter, DeriveActiveEnum)]
#[sea_orm(rs_type = "String", db_type = "Enum", enum_name = "transaction_source")]
pub enum TransactionSource {
    #[sea_orm(string_value = "manual")]
    Manual,
    #[sea_orm(string_value = "plaid")]
    Plaid,
}

/// Settlement / inclusion status (SPEC §4.4, §4.10; `BUDGET-STATUS-DRIVES-INCLUSION-1`).
///
/// - `pending`: Plaid-seen but not yet settled — **EXCLUDED** from budget math.
/// - `settled`: real transaction, confirmed — **INCLUDED**.
/// - `expected`: manual placeholder for a known future charge — **INCLUDED** (reserves budget).
#[derive(Copy, Clone, Debug, PartialEq, Eq, EnumIter, DeriveActiveEnum)]
#[sea_orm(rs_type = "String", db_type = "Enum", enum_name = "transaction_status")]
pub enum TransactionStatus {
    #[sea_orm(string_value = "pending")]
    Pending,
    #[sea_orm(string_value = "settled")]
    Settled,
    #[sea_orm(string_value = "expected")]
    Expected,
}

/// Income sub-kind for income-flow transactions (SPEC §4.8).
/// NULL for expense/rollover rows; non-null for income rows.
#[derive(Copy, Clone, Debug, PartialEq, Eq, EnumIter, DeriveActiveEnum)]
#[sea_orm(rs_type = "String", db_type = "Enum", enum_name = "income_kind")]
pub enum IncomeKind {
    /// Recurring paycheck — reconciles against expected income for the month.
    #[sea_orm(string_value = "budgeted")]
    Budgeted,
    /// Unplanned inflow (gift, refund, bonus, side gig).
    #[sea_orm(string_value = "new")]
    New,
}

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "transactions")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    pub user_id: Uuid,
    pub month_id: Uuid,
    /// NULL = uncategorized (freshly pulled from Plaid, awaiting user assignment).
    pub category_id: Option<Uuid>,
    /// NULL = no linked account (e.g., a manual entry without account selection).
    pub account_id: Option<Uuid>,
    /// Actual purchase / post date.
    pub date: Date,
    /// Signed: negative = expense, positive = inflow. Plaid amounts are flipped at
    /// the mapper boundary (`BUDGET-PLAID-SIGN-1`). NUMERIC — never f64 (`BUDGET-MONEY-1`).
    pub amount: Decimal,
    pub description: String,
    pub source: TransactionSource,
    /// Plaid stable transaction ID for dedup. UNIQUE enforced at the DB level only
    /// (ENTITIES-8 — a partial-unique sense: UNIQUE WHERE `plaid_transaction_id IS NOT NULL`).
    pub plaid_transaction_id: Option<String>,
    pub status: TransactionStatus,
    /// NULL for expense rows; non-null for income-flow rows (SPEC §4.8).
    pub income_kind: Option<IncomeKind>,
    /// True for the system-generated 1st-of-month rollover line item
    /// (`BUDGET-ROLLOVER-INTEGRITY-1`). A DB partial unique on `(month_id) WHERE is_rollover`
    /// prevents double-posting; enforced at the DB level only (ENTITIES-8).
    pub is_rollover: bool,
    /// True for a fund DRAW (surplus draw, sinking payout) that must NOT be
    /// re-charged against the month budget (`BUDGET-NO-DOUBLE-CHARGE-1` / D6 Model
    /// A). The money was already expensed at contribution time; the draw is excluded
    /// from the month expense-remaining sum.
    pub is_fund_draw: bool,
    /// The real transaction that settled this `expected` placeholder
    /// (`SPEC §4.10` / `§12`, `BUDGET-SETTLE-ON-MATCH-1`). Set ONLY on an
    /// `expected` placeholder row; NULL on every other row. Self-referential FK
    /// → `transactions(id)` with `ON DELETE SET NULL` (migration m0003): deleting
    /// the matched real txn clears the link (restoring the placeholder) rather
    /// than cascading the placeholder away. A matched placeholder is excluded
    /// from budget math so the placeholder/real pair counts exactly once
    /// (`BUDGET-NO-DOUBLE-CHARGE-1`).
    pub matched_transaction_id: Option<Uuid>,
    /// User's free-text note on this expense (`SPEC §5` / `§7`, migration m0005).
    /// Distinct from `description` (the Plaid/merchant string). NULL = no note.
    /// Inline-editable in the ledger; also settable during Pending triage.
    pub comment: Option<String>,
    pub created_at: DateTimeWithTimeZone,
    pub updated_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::users::Entity",
        from = "Column::UserId",
        to = "super::users::Column::Id",
        on_update = "NoAction",
        on_delete = "Cascade"
    )]
    User,
    #[sea_orm(
        belongs_to = "super::months::Entity",
        from = "Column::MonthId",
        to = "super::months::Column::Id",
        on_update = "NoAction",
        on_delete = "Cascade"
    )]
    Month,
    #[sea_orm(
        belongs_to = "super::categories::Entity",
        from = "Column::CategoryId",
        to = "super::categories::Column::Id",
        on_update = "NoAction",
        on_delete = "SetNull"
    )]
    Category,
    #[sea_orm(
        belongs_to = "super::accounts::Entity",
        from = "Column::AccountId",
        to = "super::accounts::Column::Id",
        on_update = "NoAction",
        on_delete = "SetNull"
    )]
    Account,
    #[sea_orm(has_many = "super::repayment_obligations::Entity")]
    RepaymentObligations,
    /// Self-referential link: the real transaction that settled this `expected`
    /// placeholder (`BUDGET-SETTLE-ON-MATCH-1`, migration m0003). `ON DELETE SET
    /// NULL` so deleting the matched real txn clears the link (the placeholder is
    /// restored), never cascading the placeholder away.
    #[sea_orm(
        belongs_to = "Entity",
        from = "Column::MatchedTransactionId",
        to = "Column::Id",
        on_update = "NoAction",
        on_delete = "SetNull"
    )]
    MatchedTransaction,
}

impl Related<super::users::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::User.def()
    }
}

impl Related<super::months::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Month.def()
    }
}

impl Related<super::categories::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Category.def()
    }
}

impl Related<super::accounts::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Account.def()
    }
}

impl Related<super::repayment_obligations::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::RepaymentObligations.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
