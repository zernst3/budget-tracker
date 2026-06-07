//! `SeaORM` entity for `categories`.
//!
//! Schema source: `SPEC.md §5` — the authoritative definition for the budget tracker.
//!
//! Pattern: per ENTITIES-1..6, ENTITIES-8, ENTITIES-12 in `agora-rs/docs/CONVENTIONS.md`.
//!
//! Categories define spending buckets within a budget version (SPEC §4.2).
//! Money columns (`amount`, `fund_balance`) use `Decimal` per `BUDGET-MONEY-1` / `DOMAIN-8`.
//!
//! Schema affordances from §12 (resolved decisions):
//!   - `category_key`: stable lineage ID across budget versions (D3). Cross-version
//!     reporting is NOT built in V1 but the column is added now.
//!   - `is_rollover_bucket`: exactly ONE per budget version, enforced by a DB partial
//!     unique index on `(budget_id) WHERE is_rollover_bucket`. Per ENTITIES-8, that
//!     partial unique is enforced at the DB level only — no `#[sea_orm(unique)]` here.
//!   - `cadence > monthly` means this is a sinking fund (SPEC §4.7); the fund accrual
//!     amount is `amount / period_months` (or the cadence's implied period).
//!
//! Per ENTITIES-4 the inverse `has_many Transactions` is declared on this side.

use sea_orm::entity::prelude::*;

/// Bucket group: predictable fixed expenses vs. discretionary spending (SPEC §4.2).
#[derive(Clone, Debug, PartialEq, Eq, EnumIter, DeriveActiveEnum)]
#[sea_orm(rs_type = "String", db_type = "Enum", enum_name = "category_grp")]
pub enum CategoryGrp {
    #[sea_orm(string_value = "fixed")]
    Fixed,
    #[sea_orm(string_value = "discretionary")]
    Discretionary,
}

/// Settle type for fixed categories (SPEC §4.2).
/// `true_set`: amount known in advance (rent, phone).
/// `flexible_set`: placeholder until real bill(s) land (utilities).
/// NULL for discretionary categories.
#[derive(Clone, Debug, PartialEq, Eq, EnumIter, DeriveActiveEnum)]
#[sea_orm(rs_type = "String", db_type = "Enum", enum_name = "settle_type")]
pub enum SettleType {
    #[sea_orm(string_value = "true_set")]
    TrueSet,
    #[sea_orm(string_value = "flexible_set")]
    FlexibleSet,
}

/// Accrual cadence (SPEC §4.7). Anything longer than monthly is a sinking fund.
#[derive(Clone, Debug, PartialEq, Eq, EnumIter, DeriveActiveEnum)]
#[sea_orm(rs_type = "String", db_type = "Enum", enum_name = "cadence")]
pub enum Cadence {
    #[sea_orm(string_value = "monthly")]
    Monthly,
    #[sea_orm(string_value = "quarterly")]
    Quarterly,
    #[sea_orm(string_value = "semiannual")]
    Semiannual,
    #[sea_orm(string_value = "annual")]
    Annual,
}

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "categories")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    pub budget_id: Uuid,
    /// Stable lineage ID across budget versions (D3, §12). Cross-version reporting
    /// is deferred to V2; column is present now so no future migration is needed.
    pub category_key: Uuid,
    pub name: String,
    /// Monthly budgeted amount. For sinking funds, accrual = `amount / period_months`.
    /// NUMERIC — never f64 (`BUDGET-MONEY-1`).
    pub amount: Decimal,
    pub grp: CategoryGrp,
    /// NULL for discretionary categories; non-null only for fixed.
    pub settle_type: Option<SettleType>,
    /// `flexible_set` only: how many transactions must be assigned before considered settled.
    pub expected_bills: Option<i32>,
    /// Exactly ONE per budget version. Enforced by a DB partial unique index on
    /// `(budget_id) WHERE is_rollover_bucket` (ENTITIES-8 / §12 D#11).
    pub is_rollover_bucket: bool,
    /// Accrual cadence. `monthly` = normal; anything longer = sinking fund (SPEC §4.7).
    pub cadence: Cadence,
    /// Arbitrary cadence override in months (NULL = use the `cadence` enum's implied period).
    pub period_months: Option<i32>,
    /// Sinking-fund carryover balance — the virtual envelope (SPEC §4.7).
    /// NUMERIC — never f64 (`BUDGET-MONEY-1`).
    pub fund_balance: Decimal,
    /// Sinking-fund next occurrence date; resets on payment to anchor the next accrual cycle.
    pub next_due_date: Option<Date>,
    /// Display ordering within the budget version.
    pub sort_order: i32,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::budgets::Entity",
        from = "Column::BudgetId",
        to = "super::budgets::Column::Id",
        on_update = "NoAction",
        on_delete = "Cascade"
    )]
    Budget,
    #[sea_orm(has_many = "super::transactions::Entity")]
    Transactions,
}

impl Related<super::budgets::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Budget.def()
    }
}

impl Related<super::transactions::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Transactions.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
