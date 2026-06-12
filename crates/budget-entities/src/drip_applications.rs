//! `SeaORM` entity for `drip_applications` (DRIP & real-time tracking, migration
//! `m0008`).
//!
//! Schema source: `docs/DRIP_REALTIME_DESIGN.md §4` — the authoritative
//! definition for the DRIP feature.
//!
//! Pattern: per ENTITIES-1..6, ENTITIES-13.
//!
//! ## Append-only system log (`SQL-AUDIT-COLUMNS-1`)
//!
//! One row per `(position, dividend pay-date)` — the position-keyed auditable
//! DRIP accretion chain (`BUDGET-ROLLOVER-INTEGRITY-1`). Rows are never updated:
//! there is an `applied_at` create timestamp but NO `updated_at`, NO
//! `created_by`/`modified_by` (machine-written, single-user), and NO reverse
//! `has_many` on `positions`/`users`. The FK to `positions` is `ON DELETE
//! Cascade` (a position's chain goes with the position).
//!
//! `shares_added` is a COUNT, `cash_added` / `amount_per_share` / `price_used`
//! are money — all NUMERIC, never f64 (`BUDGET-MONEY-1`). Exactly one of
//! `shares_added` (DRIP on) / `cash_added` (DRIP off) is non-zero per row.
//!
//! Per ENTITIES-6/7 the `(position_id, pay_date)` uniqueness (the idempotency
//! guard) and the FK + history indexes live at the DB level only
//! (`uq_drip_applications_position_pay_date`, `ix_drip_applications_position_id`,
//! `ix_drip_applications_user_applied` in m0008); the entity macro cannot express
//! a composite unique, so it is documented here.

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "drip_applications")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    pub user_id: Uuid,
    /// The position this application accreted (FK → positions, Cascade).
    pub position_id: Uuid,
    /// The ticker (denormalized for audit readability). Plain `TEXT`.
    pub ticker: String,
    /// The dividend pay-date — the idempotency key half. DATE.
    pub pay_date: Date,
    /// Dividend amount per share applied. NUMERIC money (`BUDGET-MONEY-1`).
    pub amount_per_share: Decimal,
    /// Per-share price used on the pay-date. NUMERIC money (`BUDGET-MONEY-1`).
    pub price_used: Decimal,
    /// Shares added — a COUNT, never money (`BUDGET-MONEY-1`). `0` when DRIP off.
    pub shares_added: Decimal,
    /// Cash added to the account `CashBalance` when DRIP off (`BUDGET-CASH-1`).
    /// NUMERIC money; `0` when DRIP on.
    pub cash_added: Decimal,
    /// Whether DRIP was enabled on the position at apply time.
    pub drip_on_at_apply: bool,
    /// When this application was written (`ARCH-UTC-TIMESTAMPS-1`). The single
    /// audit timestamp — no `updated_at` (append-only, `SQL-AUDIT-COLUMNS-1`).
    pub applied_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::positions::Entity",
        from = "Column::PositionId",
        to = "super::positions::Column::Id",
        on_update = "NoAction",
        on_delete = "Cascade"
    )]
    Position,
}

impl Related<super::positions::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Position.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
