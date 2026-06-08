//! `SeaORM` [`BudgetRepository`] implementation (`REPO-1`).
//!
//! Owns the [`Budget`] aggregate and its [`Category`] children (categories have
//! no independent lifecycle). Reads return domain types via the mappers
//! (`REPO-2`); writes upsert through the unit-of-work-aware executor
//! (`REPO-4`/`REPO-6`). The active-version resolution and the single
//! rollover-bucket lookup back the lazy-init flow
//! (`BUDGET-ROLLOVER-INTEGRITY-1`).

use async_trait::async_trait;
use chrono::NaiveDate;
use sea_orm::{ColumnTrait, Condition, DatabaseConnection, EntityTrait, QueryFilter, QueryOrder};

use budget_domain::RepositoryError;
use budget_domain::budget::Budget;
use budget_domain::category::Category;
use budget_domain::ids::{BudgetId, CategoryId, UserId};
use budget_domain::repositories::BudgetRepository;
use budget_domain::uow::UnitOfWork;

use budget_entities::{budgets, categories};
use budget_mappers::{budgets as budgets_mapper, categories as categories_mapper};

use crate::conn::with_conn;
use crate::error::map_db_err;
use crate::repositories::map_read;
use crate::upsert::upsert;

/// `SeaORM`-backed [`BudgetRepository`].
pub struct PostgresBudgetRepository {
    db: DatabaseConnection,
}

impl PostgresBudgetRepository {
    /// Build the repository over a connection pool.
    #[must_use]
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }

    /// Map a collection of budget models into domain budgets.
    fn map_budgets(models: Vec<budgets::Model>) -> Result<Vec<Budget>, RepositoryError> {
        models
            .into_iter()
            .map(|m| budgets_mapper::model_to_domain(m).map_err(map_read))
            .collect()
    }
}

#[async_trait]
impl BudgetRepository for PostgresBudgetRepository {
    async fn find_by_id(&self, id: BudgetId) -> Result<Option<Budget>, RepositoryError> {
        let model = budgets::Entity::find_by_id(id.value())
            .one(&self.db)
            .await
            .map_err(map_db_err)?;
        model
            .map(budgets_mapper::model_to_domain)
            .transpose()
            .map_err(map_read)
    }

    async fn find_active_for_date(
        &self,
        user_id: UserId,
        date: NaiveDate,
    ) -> Result<Option<Budget>, RepositoryError> {
        // The version whose [effective_from, effective_to] range covers `date`
        // (SPEC §4.1): effective_from <= date AND (effective_to IS NULL OR
        // effective_to >= date). NULL effective_to = the open-ended current
        // version.
        let model = budgets::Entity::find()
            .filter(budgets::Column::UserId.eq(user_id.value()))
            .filter(budgets::Column::EffectiveFrom.lte(date))
            .filter(
                Condition::any()
                    .add(budgets::Column::EffectiveTo.is_null())
                    .add(budgets::Column::EffectiveTo.gte(date)),
            )
            .order_by_desc(budgets::Column::EffectiveFrom)
            .one(&self.db)
            .await
            .map_err(map_db_err)?;
        model
            .map(budgets_mapper::model_to_domain)
            .transpose()
            .map_err(map_read)
    }

    async fn find_current(&self, user_id: UserId) -> Result<Option<Budget>, RepositoryError> {
        let model = budgets::Entity::find()
            .filter(budgets::Column::UserId.eq(user_id.value()))
            .filter(budgets::Column::EffectiveTo.is_null())
            .one(&self.db)
            .await
            .map_err(map_db_err)?;
        model
            .map(budgets_mapper::model_to_domain)
            .transpose()
            .map_err(map_read)
    }

    async fn list_for_user(&self, user_id: UserId) -> Result<Vec<Budget>, RepositoryError> {
        let models = budgets::Entity::find()
            .filter(budgets::Column::UserId.eq(user_id.value()))
            .order_by_desc(budgets::Column::EffectiveFrom)
            .all(&self.db)
            .await
            .map_err(map_db_err)?;
        Self::map_budgets(models)
    }

    async fn list_categories(&self, budget_id: BudgetId) -> Result<Vec<Category>, RepositoryError> {
        let models = categories::Entity::find()
            .filter(categories::Column::BudgetId.eq(budget_id.value()))
            .order_by_asc(categories::Column::SortOrder)
            .all(&self.db)
            .await
            .map_err(map_db_err)?;
        models
            .into_iter()
            .map(|m| categories_mapper::model_to_domain(m).map_err(map_read))
            .collect()
    }

    async fn find_category(&self, id: CategoryId) -> Result<Option<Category>, RepositoryError> {
        let model = categories::Entity::find_by_id(id.value())
            .one(&self.db)
            .await
            .map_err(map_db_err)?;
        model
            .map(categories_mapper::model_to_domain)
            .transpose()
            .map_err(map_read)
    }

    async fn find_rollover_bucket(
        &self,
        budget_id: BudgetId,
    ) -> Result<Option<Category>, RepositoryError> {
        // The single is_rollover_bucket=true category per budget version,
        // guaranteed unique by the partial unique index
        // (BUDGET-ROLLOVER-INTEGRITY-1; §12).
        let model = categories::Entity::find()
            .filter(categories::Column::BudgetId.eq(budget_id.value()))
            .filter(categories::Column::IsRolloverBucket.eq(true))
            .one(&self.db)
            .await
            .map_err(map_db_err)?;
        model
            .map(categories_mapper::model_to_domain)
            .transpose()
            .map_err(map_read)
    }

    async fn save(
        &self,
        budget: &Budget,
        uow: Option<&dyn UnitOfWork>,
    ) -> Result<(), RepositoryError> {
        let active = budgets_mapper::domain_to_active_model(budget);
        with_conn(&self.db, uow, move |conn| {
            Box::pin(async move { upsert::<budgets::Entity, _, _>(conn, active).await })
        })
        .await
    }

    async fn save_category(
        &self,
        category: &Category,
        uow: Option<&dyn UnitOfWork>,
    ) -> Result<(), RepositoryError> {
        let active = categories_mapper::domain_to_active_model(category);
        with_conn(&self.db, uow, move |conn| {
            Box::pin(async move { upsert::<categories::Entity, _, _>(conn, active).await })
        })
        .await
    }
}
