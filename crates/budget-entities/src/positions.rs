//! `SeaORM` entity for `positions` (AI Portfolio Insights, migration `m0007`).
//!
//! Schema source: `docs/AI_FEATURE_DESIGN.md §1.E/§1.F` — the authoritative
//! definition for the portfolio-insights feature.
//!
//! Pattern: per ENTITIES-1..6, ENTITIES-12 in `agora-rs/docs/CONVENTIONS.md`.
//!
//! An investment holding: a count of `shares` of one `ticker` in a labelled
//! account. `shares` is a COUNT (`NUMERIC`), never money (`BUDGET-MONEY-1`);
//! `cost_basis` is a nullable money `NUMERIC`.
//!
//! `account_type` REUSES the shared `super::accounts::AccountType` pg-enum
//! (`account_type`) — there is no `positions`-specific account-type enum.
//!
//! Per ENTITIES-6/7 the `(user_id, ticker, account_label)` uniqueness (one row
//! per holding per account) and the `user_id` FK index live at the DB level only
//! (`uq_positions_user_ticker_account`, `ix_positions_user_id` in m0007); the
//! entity macro cannot express a composite unique, so it is documented here.
//!
//! Per ENTITIES-4 the `Relation::User` belongs_to is declared on this side with
//! `ON DELETE Cascade` (a user's positions go when the user does).

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "positions")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    pub user_id: Uuid,
    /// The validated stock symbol (validated on the way into the domain via
    /// `Ticker::try_new`; stored as plain `TEXT`).
    pub ticker: String,
    /// Human label for the holding's account ("Fidelity Roth").
    pub account_label: String,
    /// Reuses the shared `account_type` pg-enum (`super::accounts::AccountType`).
    pub account_type: super::accounts::AccountType,
    /// Confirmed baseline share COUNT, never money (`BUDGET-MONEY-1`). NUMERIC.
    pub shares: Decimal,
    /// Optional cost basis. NUMERIC money — never f64 (`BUDGET-MONEY-1`).
    pub cost_basis: Option<Decimal>,
    /// Per-position DRIP toggle (Phase 7, m0008). BOOLEAN NOT NULL DEFAULT false.
    /// PERSISTS across uploads for surviving positions (§2.7/§6).
    pub drip_enabled: bool,
    /// As-of date of the confirmed `shares` baseline (Phase 7, m0008,
    /// `BUDGET-CUTOVER-1`). TIMESTAMPTZ; DRIP applies to events after it.
    pub baseline_as_of: DateTimeWithTimeZone,
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
