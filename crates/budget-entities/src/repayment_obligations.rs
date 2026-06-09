//! `SeaORM` entity for `repayment_obligations`.
//!
//! Schema source: `SPEC.md §5` — the authoritative definition for the budget tracker.
//!
//! Pattern: per ENTITIES-1..6, ENTITIES-12 in `agora-rs/docs/CONVENTIONS.md`.
//!
//! Created when the buffer funds a large purchase ("pay off in X months", SPEC §4.9 D7).
//! The full-price transaction posts immediately for accurate tracking; the budget impact
//! is the compulsory monthly installments flowing back into the buffer until `remaining = 0`.
//!
//! Two business FKs to different parents:
//!
//!   - `fund_id` → `funds` (the buffer being repaid)
//!   - `transaction_id` → `transactions` (the large purchase)
//!
//! Both declared as `belongs_to` per ENTITIES-4.
//!
//! All monetary columns use `Decimal` per `BUDGET-MONEY-1` / `DOMAIN-8`.

use sea_orm::entity::prelude::*;

/// Repayment obligation lifecycle status (SPEC §5).
#[derive(Copy, Clone, Debug, PartialEq, Eq, EnumIter, DeriveActiveEnum)]
#[sea_orm(rs_type = "String", db_type = "Enum", enum_name = "obligation_status")]
pub enum ObligationStatus {
    #[sea_orm(string_value = "active")]
    Active,
    #[sea_orm(string_value = "paid")]
    Paid,
}

/// What the obligation amortizes (SPEC §5 / §12 D9, `BUDGET-DEFICIT-FINANCING-1`):
/// a single buffer-financed large purchase, or a financed accumulated deficit.
#[derive(Copy, Clone, Debug, PartialEq, Eq, EnumIter, DeriveActiveEnum)]
#[sea_orm(rs_type = "String", db_type = "Enum", enum_name = "obligation_source")]
pub enum ObligationSource {
    #[sea_orm(string_value = "large_purchase")]
    LargePurchase,
    #[sea_orm(string_value = "deficit")]
    Deficit,
}

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "repayment_obligations")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    pub user_id: Uuid,
    /// The buffer fund being repaid.
    pub fund_id: Uuid,
    /// What the principal represents (D9, §12). Real pg enum (`ENTITIES-12`).
    pub source: ObligationSource,
    /// The large-purchase transaction (marked spent in full at purchase; D7, §12).
    /// NULL for a financed deficit (D9) — no single source transaction.
    pub transaction_id: Option<Uuid>,
    /// The closed month whose accumulated deficit was financed (D9, §12). NULL for
    /// a large purchase.
    pub origin_month_id: Option<Uuid>,
    /// Full purchase price. NUMERIC — never f64 (`BUDGET-MONEY-1`).
    pub total_amount: Decimal,
    /// Remaining to repay. NUMERIC — never f64 (`BUDGET-MONEY-1`).
    pub remaining_amount: Decimal,
    /// Compulsory monthly installment. NUMERIC — never f64 (`BUDGET-MONEY-1`).
    pub installment_amount: Decimal,
    pub months_remaining: i32,
    pub status: ObligationStatus,
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
    #[sea_orm(
        belongs_to = "super::funds::Entity",
        from = "Column::FundId",
        to = "super::funds::Column::Id",
        on_update = "NoAction",
        on_delete = "Restrict"
    )]
    Fund,
    #[sea_orm(
        belongs_to = "super::transactions::Entity",
        from = "Column::TransactionId",
        to = "super::transactions::Column::Id",
        on_update = "NoAction",
        on_delete = "Restrict"
    )]
    Transaction,
    #[sea_orm(
        belongs_to = "super::months::Entity",
        from = "Column::OriginMonthId",
        to = "super::months::Column::Id",
        on_update = "NoAction",
        on_delete = "Restrict"
    )]
    OriginMonth,
}

impl Related<super::users::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::User.def()
    }
}

impl Related<super::funds::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Fund.def()
    }
}

impl Related<super::transactions::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Transaction.def()
    }
}

impl Related<super::months::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::OriginMonth.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
