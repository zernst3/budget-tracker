//! `SeaORM`-backed [`DripApplicationRepository`] — the append-only DRIP accretion
//! chain (`REPO-1`, `SQL-AUDIT-COLUMNS-1`, `docs/DRIP_REALTIME_DESIGN.md §6`).
//!
//! The single write is [`apply_if_absent`](DripApplicationRepository::apply_if_absent),
//! which inserts under the `(position_id, pay_date)` unique guard with `ON
//! CONFLICT DO NOTHING` — so a re-entrant catch-up (two opens, a same-day reopen)
//! posts nothing extra: the exact `BUDGET-IDEMPOTENT-MONTH-INIT-1` guarantee. It
//! returns whether a row was actually inserted (rows-affected `> 0`).
//!
//! Reads ([`list_for_position`](DripApplicationRepository::list_for_position)) are
//! chronological by `pay_date` — the chain a current-shares recompute folds over
//! (`BUDGET-ROLLOVER-INTEGRITY-1`). Each row maps through
//! [`budget_mappers::drip_applications`] (fallible on a corrupt stored `ticker`).

use async_trait::async_trait;
use sea_orm::sea_query::OnConflict;
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, QueryOrder};

use budget_domain::RepositoryError;
use budget_domain::ids::PositionId;
use budget_domain::portfolio::DripApplication;
use budget_domain::repositories::DripApplicationRepository;
use budget_domain::uow::UnitOfWork;

use budget_entities::drip_applications;
use budget_mappers::drip_applications as drip_applications_mapper;

use crate::conn::with_conn;
use crate::error::map_db_err;
use crate::repositories::map_read;

/// `SeaORM`-backed [`DripApplicationRepository`] (append-only accretion chain).
pub struct PostgresDripApplicationRepository {
    db: DatabaseConnection,
}

impl PostgresDripApplicationRepository {
    /// Build the repository over a connection pool.
    #[must_use]
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }
}

#[async_trait]
impl DripApplicationRepository for PostgresDripApplicationRepository {
    async fn apply_if_absent(
        &self,
        application: &DripApplication,
        uow: Option<&dyn UnitOfWork>,
    ) -> Result<bool, RepositoryError> {
        let active = drip_applications_mapper::domain_to_active_model(application);
        with_conn(&self.db, uow, move |conn| {
            Box::pin(async move {
                // ON CONFLICT (position_id, pay_date) DO NOTHING — the idempotency
                // guard. rows_affected == 0 means the guard suppressed a duplicate.
                let on_conflict = OnConflict::columns([
                    drip_applications::Column::PositionId,
                    drip_applications::Column::PayDate,
                ])
                .do_nothing()
                .to_owned();
                let result = drip_applications::Entity::insert(active)
                    .on_conflict(on_conflict)
                    .exec_without_returning(conn)
                    .await
                    .map_err(map_db_err)?;
                Ok(result > 0)
            })
        })
        .await
    }

    async fn list_for_position(
        &self,
        position_id: PositionId,
    ) -> Result<Vec<DripApplication>, RepositoryError> {
        let rows = drip_applications::Entity::find()
            .filter(drip_applications::Column::PositionId.eq(position_id.value()))
            .order_by_asc(drip_applications::Column::PayDate)
            .all(&self.db)
            .await
            .map_err(map_db_err)?;
        rows.into_iter()
            .map(|m| drip_applications_mapper::model_to_domain(m).map_err(map_read))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use budget_domain::ids::{DripApplicationId, UserId};
    use budget_domain::money::Money;
    use budget_domain::portfolio::Ticker;
    use chrono::{NaiveDate, TimeZone, Utc};
    use sea_orm::{DatabaseBackend, MockDatabase, MockExecResult};

    fn sample(pay: NaiveDate) -> DripApplication {
        DripApplication {
            id: DripApplicationId::generate(),
            user_id: UserId::generate(),
            position_id: PositionId::generate(),
            ticker: Ticker::try_new("AAPL").unwrap(),
            pay_date: pay,
            amount_per_share: Money::from_minor(25),
            price_used: Money::from_minor(18_000),
            shares_added: rust_decimal::Decimal::new(125, 3),
            cash_added: Money::ZERO,
            drip_on_at_apply: true,
            applied_at: Utc::now(),
        }
    }

    #[tokio::test]
    async fn apply_if_absent_returns_true_on_insert() {
        let db = MockDatabase::new(DatabaseBackend::Postgres)
            .append_exec_results([MockExecResult {
                last_insert_id: 0,
                rows_affected: 1,
            }])
            .into_connection();
        let repo = PostgresDripApplicationRepository::new(db);
        let inserted = repo
            .apply_if_absent(&sample(NaiveDate::from_ymd_opt(2026, 5, 15).unwrap()), None)
            .await
            .unwrap();
        assert!(inserted, "a fresh (position, pay_date) inserts");
    }

    #[tokio::test]
    async fn apply_if_absent_returns_false_on_conflict() {
        // rows_affected == 0 => the unique guard suppressed the duplicate.
        let db = MockDatabase::new(DatabaseBackend::Postgres)
            .append_exec_results([MockExecResult {
                last_insert_id: 0,
                rows_affected: 0,
            }])
            .into_connection();
        let repo = PostgresDripApplicationRepository::new(db);
        let inserted = repo
            .apply_if_absent(&sample(NaiveDate::from_ymd_opt(2026, 5, 15).unwrap()), None)
            .await
            .unwrap();
        assert!(!inserted, "a duplicate (position, pay_date) posts nothing");
    }

    #[tokio::test]
    async fn list_for_position_maps_rows() {
        let pay = NaiveDate::from_ymd_opt(2026, 5, 15).unwrap();
        let ts = Utc.with_ymd_and_hms(2026, 5, 15, 0, 0, 0).unwrap();
        let m = drip_applications::Model {
            id: uuid::Uuid::new_v4(),
            user_id: uuid::Uuid::new_v4(),
            position_id: uuid::Uuid::new_v4(),
            ticker: "AAPL".to_owned(),
            pay_date: pay,
            amount_per_share: rust_decimal::Decimal::new(25, 2),
            price_used: rust_decimal::Decimal::new(18_000, 2),
            shares_added: rust_decimal::Decimal::new(125, 3),
            cash_added: rust_decimal::Decimal::ZERO,
            drip_on_at_apply: true,
            applied_at: ts.into(),
        };
        let db = MockDatabase::new(DatabaseBackend::Postgres)
            .append_query_results([[m]])
            .into_connection();
        let repo = PostgresDripApplicationRepository::new(db);
        let out = repo
            .list_for_position(PositionId::generate())
            .await
            .unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].shares_added, rust_decimal::Decimal::new(125, 3));
        assert!(out[0].drip_on_at_apply);
    }
}
