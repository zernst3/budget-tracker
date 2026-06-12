//! `SeaORM` entity for `dividend_events` (DRIP & real-time tracking, migration
//! `m0008`).
//!
//! Schema source: `docs/DRIP_REALTIME_DESIGN.md §4` — the authoritative
//! definition for the DRIP feature.
//!
//! Pattern: per ENTITIES-1..6, ENTITIES-13.
//!
//! A ticker-keyed dividend cache row, SHARED across positions of the same ticker
//! so a dividend is fetched once. `amount_per_share` is exact `NUMERIC`
//! (`BUDGET-MONEY-1`); `source` is the chain-tier provenance label
//! (tiingo/yahoo/manual/mock, stored as plain `TEXT`). `ex_date`/`pay_date` are
//! `DATE`. There is NO user FK: the cache is global per ticker (`§4`).
//!
//! Per ENTITIES-6/7 the `(ticker, pay_date)` uniqueness (one cache row per
//! dividend) and the `ticker` lookup index live at the DB level only
//! (`uq_dividend_events_ticker_pay_date`, `ix_dividend_events_ticker` in m0008);
//! the entity macro cannot express a composite unique, so it is documented here.

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "dividend_events")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    /// The ticker this dividend was paid on (validated on the way into the domain
    /// via `Ticker::try_new`; stored as plain `TEXT`).
    pub ticker: String,
    /// The ex-dividend date. DATE.
    pub ex_date: Date,
    /// The pay-date — the DRIP-apply key half. DATE.
    pub pay_date: Date,
    /// Cash amount per share. NUMERIC money — never f64 (`BUDGET-MONEY-1`).
    pub amount_per_share: Decimal,
    /// Chain-tier provenance label (tiingo/yahoo/manual/mock). Plain `TEXT`.
    pub source: String,
    /// When the cache row was fetched/written (`ARCH-UTC-TIMESTAMPS-1`).
    pub fetched_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
