//! `SeaORM` entity for `paycheck_config`.
//!
//! Schema source: `SPEC.md §4.8, §5` — the authoritative definition for the budget tracker.
//!
//! Pattern: per ENTITIES-1..6, ENTITIES-12 in `agora-rs/docs/CONVENTIONS.md`.
//!
//! Income setup — one per user; extension-ready for future pay-change without migration
//! (SPEC §4.8, "design-complete, build-what-you-use"). All mode fields are first-class
//! columns; only the semimonthly fixed path is actively built in V1.
//!
//! `amount` and `smoothing_buffer` use `Decimal` per `BUDGET-MONEY-1` / `DOMAIN-8`.
//!
//! Two income modes (SPEC §4.8):
//!   - `per_paycheck` (DEFAULT): expected = paychecks-this-month × amount. No buffer needed.
//!   - `smoothed`: expected = (amount × `paychecks_per_year`) ÷ 12. Requires a smoothing buffer.
//!
//! Surplus routing (SPEC §4.8): when actual deposit exceeds expected, the over-amount is
//! routed per `surplus_routing` (default rule) or a per-transaction override.

use sea_orm::entity::prelude::*;

/// How expected monthly income is computed (SPEC §4.8).
#[derive(Copy, Clone, Debug, PartialEq, Eq, EnumIter, DeriveActiveEnum)]
#[sea_orm(rs_type = "String", db_type = "Enum", enum_name = "income_mode")]
pub enum IncomeMode {
    /// Exact: expected = paychecks landing this month × amount. No averaging, no buffer.
    /// Zach's current mode (semimonthly = always 2/month → identical to smoothed for him).
    #[sea_orm(string_value = "per_paycheck")]
    PerPaycheck,
    /// Averaged: expected = (amount × `paychecks_per_year`) ÷ 12. Needs a smoothing buffer.
    #[sea_orm(string_value = "smoothed")]
    Smoothed,
}

/// Paycheck cadence (SPEC §4.8).
#[derive(Copy, Clone, Debug, PartialEq, Eq, EnumIter, DeriveActiveEnum)]
#[sea_orm(rs_type = "String", db_type = "Enum", enum_name = "paycheck_type")]
pub enum PaycheckType {
    /// 24/yr — always exactly 2 paychecks/month. Zach's current cadence.
    #[sea_orm(string_value = "semimonthly")]
    Semimonthly,
    /// 26/yr — 2–3 paychecks/month.
    #[sea_orm(string_value = "biweekly")]
    Biweekly,
    /// 52/yr — 4–5 paychecks/month.
    #[sea_orm(string_value = "weekly")]
    Weekly,
    /// Variable / hourly — leave `amount` NULL; degrades to pure actual-tracking.
    #[sea_orm(string_value = "hourly")]
    Hourly,
}

/// Default routing for over-expected income (SPEC §4.8).
#[derive(Copy, Clone, Debug, PartialEq, Eq, EnumIter, DeriveActiveEnum)]
#[sea_orm(rs_type = "String", db_type = "Enum", enum_name = "surplus_routing")]
pub enum SurplusRouting {
    /// Default: accumulate surplus in the income smoothing buffer.
    #[sea_orm(string_value = "buffer")]
    Buffer,
    /// Add surplus to this month's free-to-spend (Other bucket).
    #[sea_orm(string_value = "this_month")]
    ThisMonth,
    /// Route surplus to external savings (outside the app's tracking).
    #[sea_orm(string_value = "savings")]
    Savings,
}

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "paycheck_config")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    pub user_id: Uuid,
    pub income_mode: IncomeMode,
    pub paycheck_type: PaycheckType,
    /// Per-paycheck amount. NULL for hourly/variable — degrades to actual-tracking.
    /// NUMERIC — never f64 (`BUDGET-MONEY-1`).
    pub amount: Option<Decimal>,
    /// Next or last paycheck date; the app infers paychecks-per-month from cadence + this anchor.
    pub anchor_date: Date,
    /// Default routing for over-expected income surplus (per-transaction override also supported).
    pub surplus_routing: SurplusRouting,
    /// Income smoothing buffer for `smoothed` mode. Dormant for Zach (semimonthly → always 2/mo).
    /// NUMERIC — never f64 (`BUDGET-MONEY-1`).
    pub smoothing_buffer: Decimal,
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
