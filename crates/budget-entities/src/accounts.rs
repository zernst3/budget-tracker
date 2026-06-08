//! `SeaORM` entity for `accounts`.
//!
//! Schema source: `SPEC.md §5` — the authoritative definition for the budget tracker.
//!
//! Pattern: per ENTITIES-1..6, ENTITIES-12 in `agora-rs/docs/CONVENTIONS.md`.
//! Tracks bank accounts (Bank of America checking, credit card, etc.) linked via Plaid
//! or manually entered (SPEC §3, §6). `plaid_item_id` is nullable — accounts may
//! exist before being linked to a Plaid item or be manually tracked.
//!
//! Per ENTITIES-4 the inverse `has_many Transactions` is declared on this side.

use sea_orm::entity::prelude::*;

/// Account type (SPEC §5). Open-ended via enum; common values enumerated here.
#[derive(Copy, Clone, Debug, PartialEq, Eq, EnumIter, DeriveActiveEnum)]
#[sea_orm(rs_type = "String", db_type = "Enum", enum_name = "account_type")]
pub enum AccountType {
    #[sea_orm(string_value = "checking")]
    Checking,
    #[sea_orm(string_value = "credit")]
    Credit,
    #[sea_orm(string_value = "savings")]
    Savings,
    #[sea_orm(string_value = "investment")]
    Investment,
    #[sea_orm(string_value = "other")]
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "accounts")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    pub user_id: Uuid,
    pub name: String,
    pub r#type: AccountType,
    /// Plaid-side stable account identifier; null for manually-tracked accounts.
    pub plaid_account_id: Option<String>,
    /// FK to the institution link; null for manually-tracked accounts.
    pub plaid_item_id: Option<Uuid>,
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
        belongs_to = "super::plaid_items::Entity",
        from = "Column::PlaidItemId",
        to = "super::plaid_items::Column::Id",
        on_update = "NoAction",
        on_delete = "SetNull"
    )]
    PlaidItem,
    #[sea_orm(has_many = "super::transactions::Entity")]
    Transactions,
}

impl Related<super::users::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::User.def()
    }
}

impl Related<super::plaid_items::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::PlaidItem.def()
    }
}

impl Related<super::transactions::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Transactions.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
