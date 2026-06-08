//! `SeaORM` [`PaycheckConfigRepository`] implementation (`REPO-1`).
//!
//! One income configuration per user (`SPEC §4.8`). Reads return the domain
//! [`PaycheckConfig`] via the mapper (`REPO-2`); `save` upserts through the
//! unit-of-work-aware executor (`REPO-4`/`REPO-6`).

use async_trait::async_trait;
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter};

use budget_domain::RepositoryError;
use budget_domain::ids::UserId;
use budget_domain::paycheck_config::PaycheckConfig;
use budget_domain::repositories::PaycheckConfigRepository;
use budget_domain::uow::UnitOfWork;

use budget_entities::paycheck_config;
use budget_mappers::paycheck_config as paycheck_config_mapper;

use crate::conn::with_conn;
use crate::error::map_db_err;
use crate::repositories::map_read;
use crate::upsert::upsert;

/// `SeaORM`-backed [`PaycheckConfigRepository`].
pub struct PostgresPaycheckConfigRepository {
    db: DatabaseConnection,
}

impl PostgresPaycheckConfigRepository {
    /// Build the repository over a connection pool.
    #[must_use]
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }
}

#[async_trait]
impl PaycheckConfigRepository for PostgresPaycheckConfigRepository {
    async fn find_for_user(
        &self,
        user_id: UserId,
    ) -> Result<Option<PaycheckConfig>, RepositoryError> {
        let model = paycheck_config::Entity::find()
            .filter(paycheck_config::Column::UserId.eq(user_id.value()))
            .one(&self.db)
            .await
            .map_err(map_db_err)?;
        model
            .map(paycheck_config_mapper::model_to_domain)
            .transpose()
            .map_err(map_read)
    }

    async fn save(
        &self,
        config: &PaycheckConfig,
        uow: Option<&dyn UnitOfWork>,
    ) -> Result<(), RepositoryError> {
        let active = paycheck_config_mapper::domain_to_active_model(config);
        with_conn(&self.db, uow, move |conn| {
            Box::pin(async move { upsert::<paycheck_config::Entity, _, _>(conn, active).await })
        })
        .await
    }
}
