//! `SeaORM` [`MonthRepository`] implementation (`REPO-1`).
//!
//! Backs lazy month-init (`BUDGET-IDEMPOTENT-MONTH-INIT-1`): find-latest anchors
//! multi-month catch-up, find-by-(year,month) checks existence, and
//! [`create_if_absent`](MonthRepository::create_if_absent) relies on the
//! `UNIQUE(user_id, year, month)` index so two racing container-wake inits
//! produce exactly one row.

use async_trait::async_trait;
use sea_orm::sea_query::OnConflict;
use sea_orm::{
    ColumnTrait, ConnectionTrait, DatabaseConnection, EntityTrait, QueryFilter, QueryOrder,
};

use budget_domain::RepositoryError;
use budget_domain::ids::{MonthId, UserId};
use budget_domain::month::Month;
use budget_domain::repositories::MonthRepository;
use budget_domain::uow::UnitOfWork;

use budget_entities::months;
use budget_mappers::months as months_mapper;

use crate::conn::with_conn;
use crate::error::map_db_err;
use crate::repositories::map_read;
use crate::upsert::upsert;

/// `SeaORM`-backed [`MonthRepository`].
pub struct PostgresMonthRepository {
    db: DatabaseConnection,
}

impl PostgresMonthRepository {
    /// Build the repository over a connection pool.
    #[must_use]
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }
}

/// Fetch a month by `(user_id, year, month)` against any executor, mapping into
/// the domain type. Shared by the public lookup and the post-insert re-read in
/// `create_if_absent`.
async fn find_by_ym<C: ConnectionTrait>(
    conn: &C,
    user_id: UserId,
    year: i32,
    month: i32,
) -> Result<Option<Month>, RepositoryError> {
    let model = months::Entity::find()
        .filter(months::Column::UserId.eq(user_id.value()))
        .filter(months::Column::Year.eq(year))
        .filter(months::Column::Month.eq(month))
        .one(conn)
        .await
        .map_err(map_db_err)?;
    model
        .map(months_mapper::model_to_domain)
        .transpose()
        .map_err(map_read)
}

#[async_trait]
impl MonthRepository for PostgresMonthRepository {
    async fn find_by_id(&self, id: MonthId) -> Result<Option<Month>, RepositoryError> {
        let model = months::Entity::find_by_id(id.value())
            .one(&self.db)
            .await
            .map_err(map_db_err)?;
        model
            .map(months_mapper::model_to_domain)
            .transpose()
            .map_err(map_read)
    }

    async fn find_by_year_month(
        &self,
        user_id: UserId,
        year: i32,
        month: i32,
    ) -> Result<Option<Month>, RepositoryError> {
        find_by_ym(&self.db, user_id, year, month).await
    }

    async fn find_latest(&self, user_id: UserId) -> Result<Option<Month>, RepositoryError> {
        // The max (year, month) — the anchor for multi-month catch-up. Order by
        // year then month, both descending; supported by ix_months_user_id plus
        // the (year, month) sort.
        let model = months::Entity::find()
            .filter(months::Column::UserId.eq(user_id.value()))
            .order_by_desc(months::Column::Year)
            .order_by_desc(months::Column::Month)
            .one(&self.db)
            .await
            .map_err(map_db_err)?;
        model
            .map(months_mapper::model_to_domain)
            .transpose()
            .map_err(map_read)
    }

    async fn list_for_user(&self, user_id: UserId) -> Result<Vec<Month>, RepositoryError> {
        let models = months::Entity::find()
            .filter(months::Column::UserId.eq(user_id.value()))
            .order_by_asc(months::Column::Year)
            .order_by_asc(months::Column::Month)
            .all(&self.db)
            .await
            .map_err(map_db_err)?;
        models
            .into_iter()
            .map(|m| months_mapper::model_to_domain(m).map_err(map_read))
            .collect()
    }

    async fn create_if_absent(
        &self,
        month: &Month,
        uow: Option<&dyn UnitOfWork>,
    ) -> Result<Month, RepositoryError> {
        // BUDGET-IDEMPOTENT-MONTH-INIT-1: INSERT ... ON CONFLICT (user_id, year,
        // month) DO NOTHING, then read back the row that now exists (whether ours
        // or a racing init's). The UNIQUE(user_id, year, month) index is the
        // arbiter, so concurrent lazy-init calls converge on one row.
        let user_id = month.user_id;
        let year = month.year;
        let month_num = month.month;
        let active = months_mapper::domain_to_active_model(month);

        let resolved = with_conn(&self.db, uow, move |conn| {
            Box::pin(async move {
                let mut on_conflict = OnConflict::columns([
                    months::Column::UserId,
                    months::Column::Year,
                    months::Column::Month,
                ]);
                on_conflict.do_nothing();

                // do_nothing with a returning insert errors on conflict in some
                // drivers; use exec_without_returning so a conflict is a no-op.
                months::Entity::insert(active)
                    .on_conflict(on_conflict)
                    .exec_without_returning(conn)
                    .await
                    .map_err(map_db_err)?;

                find_by_ym(conn, user_id, year, month_num).await
            })
        })
        .await?;

        resolved.ok_or_else(|| {
            RepositoryError::Database(
                "create_if_absent: month row absent immediately after upsert".to_string(),
            )
        })
    }

    async fn save(
        &self,
        month: &Month,
        uow: Option<&dyn UnitOfWork>,
    ) -> Result<(), RepositoryError> {
        let active = months_mapper::domain_to_active_model(month);
        with_conn(&self.db, uow, move |conn| {
            Box::pin(async move { upsert::<months::Entity, _, _>(conn, active).await })
        })
        .await
    }
}
