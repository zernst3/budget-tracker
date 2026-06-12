//! `SeaORM`-backed [`DividendEventCache`] — the ticker-keyed dividend cache
//! (`REPO-1`, `docs/DRIP_REALTIME_DESIGN.md §4`).
//!
//! A dividend is fetched once per ticker and shared across every position holding
//! it. [`upsert_many`](DividendEventCache::upsert_many) is idempotent on the
//! `(ticker, pay_date)` natural key (an `ON CONFLICT ... DO UPDATE` refreshing the
//! amount/source/fetched_at), and [`find_since`](DividendEventCache::find_since)
//! returns the cached events with `pay_date > since`, chronological — the catch-up
//! engine's read path. Each row maps through [`budget_mappers::dividend_events`]
//! (fallible on a corrupt stored `ticker`/`source`, surfaced via [`map_read`]).

use async_trait::async_trait;
use chrono::NaiveDate;
use sea_orm::sea_query::OnConflict;
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, QueryOrder};

use budget_domain::RepositoryError;
use budget_domain::portfolio::{DividendEvent, Ticker};
use budget_domain::repositories::DividendEventCache;

use budget_entities::dividend_events;
use budget_mappers::dividend_events as dividend_events_mapper;

use crate::error::map_db_err;
use crate::repositories::map_read;

/// `SeaORM`-backed [`DividendEventCache`].
pub struct PostgresDividendEventCache {
    db: DatabaseConnection,
}

impl PostgresDividendEventCache {
    /// Build the cache over a connection pool.
    #[must_use]
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }
}

#[async_trait]
impl DividendEventCache for PostgresDividendEventCache {
    async fn find_since(
        &self,
        ticker: &Ticker,
        since: NaiveDate,
    ) -> Result<Vec<DividendEvent>, RepositoryError> {
        let rows = dividend_events::Entity::find()
            .filter(dividend_events::Column::Ticker.eq(ticker.as_str()))
            .filter(dividend_events::Column::PayDate.gt(since))
            .order_by_asc(dividend_events::Column::PayDate)
            .all(&self.db)
            .await
            .map_err(map_db_err)?;
        rows.into_iter()
            .map(|m| dividend_events_mapper::model_to_domain(m).map_err(map_read))
            .collect()
    }

    async fn upsert_many(&self, events: &[DividendEvent]) -> Result<(), RepositoryError> {
        if events.is_empty() {
            return Ok(());
        }
        let now = chrono::Utc::now();
        let models: Vec<dividend_events::ActiveModel> = events
            .iter()
            .map(|e| dividend_events_mapper::to_active_model(uuid::Uuid::new_v4(), e, now))
            .collect();
        // Idempotent on the (ticker, pay_date) natural key: refresh the amount /
        // source / fetched_at for an existing key, never duplicating a row.
        let on_conflict = OnConflict::columns([
            dividend_events::Column::Ticker,
            dividend_events::Column::PayDate,
        ])
        .update_columns([
            dividend_events::Column::AmountPerShare,
            dividend_events::Column::Source,
            dividend_events::Column::FetchedAt,
        ])
        .to_owned();
        dividend_events::Entity::insert_many(models)
            .on_conflict(on_conflict)
            .exec_without_returning(&self.db)
            .await
            .map_err(map_db_err)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use budget_domain::money::Money;
    use budget_domain::portfolio::DividendSourceKind;
    use chrono::TimeZone;
    use sea_orm::{DatabaseBackend, MockDatabase, MockExecResult};

    fn ticker(s: &str) -> Ticker {
        Ticker::try_new(s).unwrap()
    }

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    fn model(pay: NaiveDate, cents: i64) -> dividend_events::Model {
        let ts = chrono::Utc.with_ymd_and_hms(2026, 5, 15, 0, 0, 0).unwrap();
        dividend_events::Model {
            id: uuid::Uuid::new_v4(),
            ticker: "AAPL".to_owned(),
            ex_date: pay - chrono::Duration::days(7),
            pay_date: pay,
            amount_per_share: rust_decimal::Decimal::new(cents, 2),
            source: "tiingo".to_owned(),
            fetched_at: ts.into(),
        }
    }

    #[tokio::test]
    async fn find_since_maps_rows_to_domain() {
        let db = MockDatabase::new(DatabaseBackend::Postgres)
            .append_query_results([[model(date(2026, 5, 15), 25)]])
            .into_connection();
        let cache = PostgresDividendEventCache::new(db);
        let out = cache
            .find_since(&ticker("AAPL"), date(2026, 1, 1))
            .await
            .unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].amount_per_share, Money::from_minor(25));
        assert_eq!(out[0].source, DividendSourceKind::Tiingo);
    }

    #[tokio::test]
    async fn upsert_many_empty_is_noop() {
        let db = MockDatabase::new(DatabaseBackend::Postgres).into_connection();
        let cache = PostgresDividendEventCache::new(db);
        assert!(cache.upsert_many(&[]).await.is_ok());
    }

    #[tokio::test]
    async fn upsert_many_issues_insert() {
        let db = MockDatabase::new(DatabaseBackend::Postgres)
            .append_exec_results([MockExecResult {
                last_insert_id: 0,
                rows_affected: 2,
            }])
            .into_connection();
        let cache = PostgresDividendEventCache::new(db);
        let events = vec![
            DividendEvent {
                ticker: ticker("AAPL"),
                ex_date: date(2026, 2, 3),
                pay_date: date(2026, 2, 10),
                amount_per_share: Money::from_minor(24),
                source: DividendSourceKind::Tiingo,
            },
            DividendEvent {
                ticker: ticker("AAPL"),
                ex_date: date(2026, 5, 8),
                pay_date: date(2026, 5, 15),
                amount_per_share: Money::from_minor(25),
                source: DividendSourceKind::Tiingo,
            },
        ];
        assert!(cache.upsert_many(&events).await.is_ok());
    }
}
