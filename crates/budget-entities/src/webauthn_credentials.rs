//! `SeaORM` entity for `webauthn_credentials`.
//!
//! Schema source: `SPEC.md §5` (the `webauthn_credentials` table) and `§9.1`
//! (`BUDGET-AUTH-GATE-1`): passkeys / biometric login. One user, many devices.
//! Materialized by the `m0002_auth_schema` migration in `budget-migration`.
//!
//! `credential_id` is UNIQUE; per ENTITIES-7 a single-column unconditional unique
//! is expressible via `#[sea_orm(unique)]`, but the canonical unique index lives
//! in the migration alongside the FK index, so this entity does NOT annotate the
//! column `unique` (the migration is the source of truth and avoids implying a
//! second, entity-derived constraint).
//!
//! Per ENTITIES-4 the FK relation is declared on both sides: `belongs_to User`
//! here, `has_many WebauthnCredentials` on `users` (added in that entity).
//!
//! `public_key` / `credential_id` are opaque authenticator-chosen byte strings
//! (`BYTEA`), stored as `Vec<u8>`.

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "webauthn_credentials")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    pub user_id: Uuid,
    /// The authenticator-assigned credential id (opaque, UNIQUE at the DB level).
    pub credential_id: Vec<u8>,
    /// The serialized passkey / public-key record (opaque blob).
    pub public_key: Vec<u8>,
    /// The authenticator signature counter at last use (clone detection).
    pub sign_count: i64,
    pub transports: Option<String>,
    pub aaguid: Option<String>,
    pub nickname: Option<String>,
    pub created_at: DateTimeWithTimeZone,
    pub last_used_at: Option<DateTimeWithTimeZone>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::users::Entity",
        from = "Column::UserId",
        to = "super::users::Column::Id",
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
