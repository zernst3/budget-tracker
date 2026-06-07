//! `SeaORM` entity for `budgets`.
//!
//! Schema source: `SPEC.md §5` — the authoritative definition for the budget tracker.
//!
//! Pattern: per ENTITIES-1..6 in `agora-rs/docs/CONVENTIONS.md`.
//! Budgets are versioned config records (SPEC §4.1). A month REFERENCES the
//! budget version active for it via FK (`months.budget_id`). Editing the budget
//! creates/advances a version; past months keep their referenced version so
//! history stays accurate.
//!
//! `effective_to = NULL` means this is the current active version.
//!
//! Per ENTITIES-4 the inverse relations are declared on this side:
//!   - `has_many Categories`, `has_many Months`.

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "budgets")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    pub user_id: Uuid,
    pub name: String,
    pub effective_from: Date,
    /// NULL = current active version; non-null = retired version.
    pub effective_to: Option<Date>,
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
    #[sea_orm(has_many = "super::categories::Entity")]
    Categories,
    #[sea_orm(has_many = "super::months::Entity")]
    Months,
}

impl Related<super::users::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::User.def()
    }
}

impl Related<super::categories::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Categories.def()
    }
}

impl Related<super::months::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Months.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
