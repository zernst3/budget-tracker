//! `SeaORM` entity for `months`.
//!
//! Schema source: `SPEC.md §5` — the authoritative definition for the budget tracker.
//!
//! Pattern: per ENTITIES-1..6, ENTITIES-7, ENTITIES-12 in `agora-rs/docs/CONVENTIONS.md`.
//!
//! Months are DB items with a lifecycle (`open` / `closed`) and reference a budget version
//! by FK (not a copy — SPEC §4.1). Month-membership is computed in the fixed home TZ
//! `America/New_York`; timestamps are stored UTC (D2, §12; `ARCH-UTC-TIMESTAMPS-1`).
//!
//! The Drizzle table carries a composite unique on `(user_id, year, month)` (SPEC §5).
//! Per ENTITIES-7, that constraint is enforced at the DB level only.
//!
//! Lazy-init (`BUDGET-IDEMPOTENT-MONTH-INIT-1`): on access, missing months are created
//! in chronological order. The UNIQUE constraint makes re-entry idempotent.
//!
//! Per ENTITIES-4 the inverse `has_many Transactions` is declared on this side.

use sea_orm::entity::prelude::*;

/// Month lifecycle status (SPEC §4.6).
#[derive(Clone, Debug, PartialEq, Eq, EnumIter, DeriveActiveEnum)]
#[sea_orm(rs_type = "String", db_type = "Enum", enum_name = "month_status")]
pub enum MonthStatus {
    #[sea_orm(string_value = "open")]
    Open,
    #[sea_orm(string_value = "closed")]
    Closed,
}

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "months")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    pub user_id: Uuid,
    /// References the budget version active for this month (not a copy — SPEC §4.1).
    pub budget_id: Uuid,
    /// Calendar year (e.g. 2026). Month-membership computed in `America/New_York`.
    pub year: i32,
    /// Calendar month, 1–12. Month-membership computed in `America/New_York`.
    pub month: i32,
    pub status: MonthStatus,
    pub opened_at: DateTimeWithTimeZone,
    pub closed_at: Option<DateTimeWithTimeZone>,
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
        belongs_to = "super::budgets::Entity",
        from = "Column::BudgetId",
        to = "super::budgets::Column::Id",
        on_update = "NoAction",
        on_delete = "Restrict"
    )]
    Budget,
    #[sea_orm(has_many = "super::transactions::Entity")]
    Transactions,
}

impl Related<super::users::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::User.def()
    }
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
