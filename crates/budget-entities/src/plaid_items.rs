//! `SeaORM` entity for `plaid_items`.
//!
//! Schema source: `SPEC.md §5` — the authoritative definition for the budget tracker.
//!
//! Pattern: per ENTITIES-1..6 in `agora-rs/docs/CONVENTIONS.md`.
//! One `plaid_item` per linked institution (SPEC §6). The Plaid `access_token` is NEVER
//! stored raw — `access_token_ref` holds a Key Vault secret reference only
//! (`BUDGET-PLAID-TOKEN-VAULT-1`). The incremental sync cursor lives here (`sync_cursor`).
//!
//! Per ENTITIES-4 the inverse `has_many Accounts` is declared on this side.

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "plaid_items")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    pub user_id: Uuid,
    pub institution_name: String,
    /// Azure Key Vault secret reference — NEVER the raw Plaid access token.
    /// (`BUDGET-PLAID-TOKEN-VAULT-1`)
    pub access_token_ref: String,
    /// Plaid cursor for incremental `/transactions/sync` pulls (SPEC §6).
    pub sync_cursor: Option<String>,
    pub last_synced_at: Option<DateTimeWithTimeZone>,
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
    #[sea_orm(has_many = "super::accounts::Entity")]
    Accounts,
}

impl Related<super::users::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::User.def()
    }
}

impl Related<super::accounts::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Accounts.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
