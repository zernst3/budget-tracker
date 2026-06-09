//! `SeaORM` [`FundRepository`] implementation (`REPO-1`).
//!
//! Owns the [`Fund`] aggregate and its [`RepaymentObligation`] children
//! (obligations are created/closed only alongside fund draws and repayments).
//! Reads return domain types via the mappers (`REPO-2`); writes upsert through
//! the unit-of-work-aware executor (`REPO-4`/`REPO-6`). The active-obligations
//! read backs the monthly compulsory installment and the buffer-health flag
//! (`SPEC §4.9`).

use async_trait::async_trait;
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, QueryOrder, QuerySelect};
use uuid::Uuid;

use budget_domain::RepositoryError;
use budget_domain::fund::Fund;
use budget_domain::ids::{FundId, MonthId, RepaymentObligationId, TransactionId, UserId};
use budget_domain::repayment_obligation::RepaymentObligation;
use budget_domain::repositories::FundRepository;
use budget_domain::uow::UnitOfWork;

use budget_entities::funds;
use budget_entities::repayment_obligations::{
    self, ObligationSource as EntityObligationSource, ObligationStatus as EntityObligationStatus,
};
use budget_mappers::{
    funds as funds_mapper, repayment_obligations as repayment_obligations_mapper,
};

use crate::conn::with_conn;
use crate::error::map_db_err;
use crate::repositories::map_read;
use crate::upsert::upsert;

/// `SeaORM`-backed [`FundRepository`].
pub struct PostgresFundRepository {
    db: DatabaseConnection,
}

impl PostgresFundRepository {
    /// Build the repository over a connection pool.
    #[must_use]
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }
}

#[async_trait]
impl FundRepository for PostgresFundRepository {
    async fn find_by_id(&self, id: FundId) -> Result<Option<Fund>, RepositoryError> {
        let model = funds::Entity::find_by_id(id.value())
            .one(&self.db)
            .await
            .map_err(map_db_err)?;
        model
            .map(funds_mapper::model_to_domain)
            .transpose()
            .map_err(map_read)
    }

    async fn list_for_user(&self, user_id: UserId) -> Result<Vec<Fund>, RepositoryError> {
        let models = funds::Entity::find()
            .filter(funds::Column::UserId.eq(user_id.value()))
            .order_by_asc(funds::Column::CreatedAt)
            .all(&self.db)
            .await
            .map_err(map_db_err)?;
        models
            .into_iter()
            .map(|m| funds_mapper::model_to_domain(m).map_err(map_read))
            .collect()
    }

    async fn save(&self, fund: &Fund, uow: Option<&dyn UnitOfWork>) -> Result<(), RepositoryError> {
        let active = funds_mapper::domain_to_active_model(fund);
        with_conn(&self.db, uow, move |conn| {
            Box::pin(async move { upsert::<funds::Entity, _, _>(conn, active).await })
        })
        .await
    }

    async fn find_obligation(
        &self,
        id: RepaymentObligationId,
    ) -> Result<Option<RepaymentObligation>, RepositoryError> {
        let model = repayment_obligations::Entity::find_by_id(id.value())
            .one(&self.db)
            .await
            .map_err(map_db_err)?;
        model
            .map(repayment_obligations_mapper::model_to_domain)
            .transpose()
            .map_err(map_read)
    }

    async fn list_active_obligations(
        &self,
        user_id: UserId,
    ) -> Result<Vec<RepaymentObligation>, RepositoryError> {
        // status = 'active' only; backed by the active-obligation partial index
        // (ix_repayment_obligations_fund_active). SPEC §4.9.
        let models = repayment_obligations::Entity::find()
            .filter(repayment_obligations::Column::UserId.eq(user_id.value()))
            .filter(repayment_obligations::Column::Status.eq(EntityObligationStatus::Active))
            .order_by_asc(repayment_obligations::Column::CreatedAt)
            .all(&self.db)
            .await
            .map_err(map_db_err)?;
        models
            .into_iter()
            .map(|m| repayment_obligations_mapper::model_to_domain(m).map_err(map_read))
            .collect()
    }

    async fn find_obligation_for_transaction(
        &self,
        transaction_id: TransactionId,
    ) -> Result<Option<RepaymentObligation>, RepositoryError> {
        let model = repayment_obligations::Entity::find()
            .filter(repayment_obligations::Column::TransactionId.eq(transaction_id.value()))
            .one(&self.db)
            .await
            .map_err(map_db_err)?;
        model
            .map(repayment_obligations_mapper::model_to_domain)
            .transpose()
            .map_err(map_read)
    }

    async fn find_active_deficit_obligation_for_month(
        &self,
        month_id: MonthId,
    ) -> Result<Option<RepaymentObligation>, RepositoryError> {
        // At most one active source='deficit' obligation per origin month (D9);
        // backed by ix_repayment_obligations_origin_month_id.
        let model = repayment_obligations::Entity::find()
            .filter(repayment_obligations::Column::OriginMonthId.eq(month_id.value()))
            .filter(repayment_obligations::Column::Source.eq(EntityObligationSource::Deficit))
            .filter(repayment_obligations::Column::Status.eq(EntityObligationStatus::Active))
            .one(&self.db)
            .await
            .map_err(map_db_err)?;
        model
            .map(repayment_obligations_mapper::model_to_domain)
            .transpose()
            .map_err(map_read)
    }

    async fn list_buffer_financed_transaction_ids(
        &self,
        user_id: UserId,
    ) -> Result<Vec<TransactionId>, RepositoryError> {
        // Every large-purchase obligation's transaction_id, active OR paid — the
        // full-price buffer-financed rows stay excluded permanently (SPEC §4.9 D7).
        // Deficit obligations (D9) have NULL transaction_id (no single source row),
        // so the IS NOT NULL filter skips them; selecting only the id column avoids
        // materialising the whole row; backed by ix_repayment_obligations_user_id.
        let ids = repayment_obligations::Entity::find()
            .filter(repayment_obligations::Column::UserId.eq(user_id.value()))
            .filter(repayment_obligations::Column::TransactionId.is_not_null())
            .select_only()
            .column(repayment_obligations::Column::TransactionId)
            .into_tuple::<Option<Uuid>>()
            .all(&self.db)
            .await
            .map_err(map_db_err)?;
        Ok(ids.into_iter().flatten().map(TransactionId::new).collect())
    }

    async fn save_obligation(
        &self,
        obligation: &RepaymentObligation,
        uow: Option<&dyn UnitOfWork>,
    ) -> Result<(), RepositoryError> {
        let active = repayment_obligations_mapper::domain_to_active_model(obligation);
        with_conn(&self.db, uow, move |conn| {
            Box::pin(
                async move { upsert::<repayment_obligations::Entity, _, _>(conn, active).await },
            )
        })
        .await
    }
}
