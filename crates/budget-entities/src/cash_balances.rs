//! `SeaORM` entity for `cash_balances` (AI Portfolio Insights, migration `m0007`).
//!
//! Schema source: `docs/AI_FEATURE_DESIGN.md §1.E/§1.F` — the authoritative
//! definition for the portfolio-insights feature.
//!
//! Pattern: per ENTITIES-1..6 in `agora-rs/docs/CONVENTIONS.md`.
//!
//! A cash balance in a labelled account. `balance` is a BALANCE (a stock), never
//! a flow (`BUDGET-CASH-1`); stored as `NUMERIC` money — never f64
//! (`BUDGET-MONEY-1`). `reserved` marks a non-investable reserve (an emergency
//! buffer) and sums into the snapshot's buffer total.
//!
//! Per ENTITIES-6/7 the `(user_id, account_label)` uniqueness (one balance per
//! account label per user) and the `user_id` FK index live at the DB level only
//! (`uq_cash_balances_user_account`, `ix_cash_balances_user_id` in m0007); the
//! entity macro cannot express a composite unique, so it is documented here.
//!
//! Per ENTITIES-4 the `Relation::User` belongs_to is declared on this side with
//! `ON DELETE Cascade`.

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "cash_balances")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    pub user_id: Uuid,
    /// Human label for the cash account.
    pub account_label: String,
    /// The cash balance — a stock, never a flow (`BUDGET-CASH-1`). NUMERIC money
    /// — never f64 (`BUDGET-MONEY-1`).
    pub balance: Decimal,
    /// `true` => a buffer / non-investable reserve (sums into `buffer_total`).
    pub reserved: bool,
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
}

impl Related<super::users::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::User.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
