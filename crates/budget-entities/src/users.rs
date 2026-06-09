//! `SeaORM` entity for `users`.
//!
//! Schema source: `SPEC.md §5` — the authoritative definition for the budget tracker.
//!
//! Pattern: per ENTITIES-1..6, ENTITIES-12 in `agora-rs/docs/CONVENTIONS.md`.
//! Single user (V1), but every core table has `user_id` for future-proofing (SPEC §5).
//!
//! `tracking_start_date` is the genesis boundary (D8, §12; `BUDGET-CUTOVER-1`):
//! everything dated before it is CLOSED and represented solely by the onboarding
//! opening snapshot. Plaid never ingests pre-this-date transactions.
//!
//! Per ENTITIES-4 the inverse relations are declared on this side:
//!   - `has_many Budgets`, `has_many Accounts`, `has_many PlaidItems`,
//!     `has_many Months`, `has_many Transactions`, `has_many Funds`,
//!     `has_many RepaymentObligations`, `has_one PaycheckConfig`,
//!     `has_many WebauthnCredentials`.

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "users")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    pub email: String,
    pub password_hash: String,
    pub totp_secret: Option<String>,
    /// Genesis cutover date (`BUDGET-CUTOVER-1`). Everything before this date is CLOSED;
    /// Plaid sync clamps its lower bound to `max(today − 30d, tracking_start_date)`.
    pub tracking_start_date: Date,
    pub created_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(has_many = "super::budgets::Entity")]
    Budgets,
    #[sea_orm(has_many = "super::accounts::Entity")]
    Accounts,
    #[sea_orm(has_many = "super::plaid_items::Entity")]
    PlaidItems,
    #[sea_orm(has_many = "super::months::Entity")]
    Months,
    #[sea_orm(has_many = "super::transactions::Entity")]
    Transactions,
    #[sea_orm(has_many = "super::funds::Entity")]
    Funds,
    #[sea_orm(has_many = "super::repayment_obligations::Entity")]
    RepaymentObligations,
    #[sea_orm(has_one = "super::paycheck_config::Entity")]
    PaycheckConfig,
    #[sea_orm(has_many = "super::webauthn_credentials::Entity")]
    WebauthnCredentials,
}

impl Related<super::budgets::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Budgets.def()
    }
}

impl Related<super::accounts::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Accounts.def()
    }
}

impl Related<super::plaid_items::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::PlaidItems.def()
    }
}

impl Related<super::months::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Months.def()
    }
}

impl Related<super::transactions::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Transactions.def()
    }
}

impl Related<super::funds::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Funds.def()
    }
}

impl Related<super::repayment_obligations::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::RepaymentObligations.def()
    }
}

impl Related<super::paycheck_config::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::PaycheckConfig.def()
    }
}

impl Related<super::webauthn_credentials::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::WebauthnCredentials.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
