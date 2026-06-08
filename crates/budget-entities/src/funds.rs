//! `SeaORM` entity for `funds`.
//!
//! Schema source: `SPEC.md §5` — the authoritative definition for the budget tracker.
//!
//! Pattern: per ENTITIES-1..6, ENTITIES-12 in `agora-rs/docs/CONVENTIONS.md`.
//!
//! Funds are the virtual-envelope primitive (SPEC §4.9):
//!   - `buffer`: tappable, `compulsory_repayment = true`; drawing creates a
//!     `repayment_obligations` row. Has a lean `target_balance`; excess → market.
//!   - `surplus`: tappable, `compulsory_repayment = false`; saved toward a specific
//!     planned purchase. No repayment obligation on draw.
//!
//! Note: sinking-fund carryover (`cadence > monthly`) is tracked as `categories.fund_balance`,
//! not as a `funds` row — sinking funds are category-attached, not standalone.
//!
//! `balance` and `target_balance` use `Decimal` per `BUDGET-MONEY-1` / `DOMAIN-8`.
//!
//! `BUDGET-FUND-EARMARK-1`: money moved INTO a fund is an expense against the month;
//! it is excluded from the rollover net so an earmarked dollar is never counted twice.
//!
//! Per ENTITIES-4 the inverse `has_many RepaymentObligations` is declared on this side.

use sea_orm::entity::prelude::*;

/// Fund kind (SPEC §4.9).
#[derive(Copy, Clone, Debug, PartialEq, Eq, EnumIter, DeriveActiveEnum)]
#[sea_orm(rs_type = "String", db_type = "Enum", enum_name = "fund_kind")]
pub enum FundKind {
    /// Emergency / working savings pool. `compulsory_repayment = true`.
    /// Drawing creates a `repayment_obligations` row; repayment restores to `target_balance`.
    #[sea_orm(string_value = "buffer")]
    Buffer,
    /// Deliberate surplus saved toward a specific planned purchase.
    /// `compulsory_repayment = false`. Draw is a fund-draw, not a re-charged budget expense.
    #[sea_orm(string_value = "surplus")]
    Surplus,
}

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "funds")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    pub user_id: Uuid,
    pub name: String,
    pub kind: FundKind,
    /// Current balance. NUMERIC — never f64 (`BUDGET-MONEY-1`).
    pub balance: Decimal,
    /// Buffer-only: lean target; app flags when balance > target (excess to invest externally)
    /// or balance < target with outstanding obligations (don't stack another draw).
    /// NULL for surplus funds. NUMERIC — never f64 (`BUDGET-MONEY-1`).
    pub target_balance: Option<Decimal>,
    /// `true` for buffer (compulsory repayment); `false` for surplus (no repayment).
    pub compulsory_repayment: bool,
    pub created_at: DateTimeWithTimeZone,
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
    #[sea_orm(has_many = "super::repayment_obligations::Entity")]
    RepaymentObligations,
}

impl Related<super::users::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::User.def()
    }
}

impl Related<super::repayment_obligations::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::RepaymentObligations.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
