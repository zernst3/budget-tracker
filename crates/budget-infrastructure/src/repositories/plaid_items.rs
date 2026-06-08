//! `SeaORM` [`PlaidItemRepository`] implementation (`REPO-1`).
//!
//! Owns the [`PlaidItem`] aggregate and its [`Account`] children, plus the
//! incremental-sync cursor read/write that `/transactions/sync` depends on
//! (`SPEC §6`). The access token is stored only as a Key Vault reference
//! (`BUDGET-PLAID-TOKEN-VAULT-1`); this repository never sees the raw token.

use async_trait::async_trait;
use chrono::Utc;
use sea_orm::ActiveValue::Set;
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, QueryOrder};

use budget_domain::RepositoryError;
use budget_domain::account::Account;
use budget_domain::ids::{AccountId, PlaidItemId, UserId};
use budget_domain::plaid_item::PlaidItem;
use budget_domain::repositories::PlaidItemRepository;
use budget_domain::uow::UnitOfWork;

use budget_entities::{accounts, plaid_items};
use budget_mappers::{accounts as accounts_mapper, plaid_items as plaid_items_mapper};

use crate::conn::with_conn;
use crate::error::map_db_err;
use crate::repositories::map_read;
use crate::upsert::upsert;

/// `SeaORM`-backed [`PlaidItemRepository`].
pub struct PostgresPlaidItemRepository {
    db: DatabaseConnection,
}

impl PostgresPlaidItemRepository {
    /// Build the repository over a connection pool.
    #[must_use]
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }
}

#[async_trait]
impl PlaidItemRepository for PostgresPlaidItemRepository {
    async fn find_by_id(&self, id: PlaidItemId) -> Result<Option<PlaidItem>, RepositoryError> {
        let model = plaid_items::Entity::find_by_id(id.value())
            .one(&self.db)
            .await
            .map_err(map_db_err)?;
        model
            .map(plaid_items_mapper::model_to_domain)
            .transpose()
            .map_err(map_read)
    }

    async fn list_for_user(&self, user_id: UserId) -> Result<Vec<PlaidItem>, RepositoryError> {
        let models = plaid_items::Entity::find()
            .filter(plaid_items::Column::UserId.eq(user_id.value()))
            .order_by_asc(plaid_items::Column::CreatedAt)
            .all(&self.db)
            .await
            .map_err(map_db_err)?;
        models
            .into_iter()
            .map(|m| plaid_items_mapper::model_to_domain(m).map_err(map_read))
            .collect()
    }

    async fn get_sync_cursor(&self, id: PlaidItemId) -> Result<Option<String>, RepositoryError> {
        // Only the cursor column is needed; select it directly rather than the
        // whole row. `None` before the first sync.
        let row = plaid_items::Entity::find_by_id(id.value())
            .one(&self.db)
            .await
            .map_err(map_db_err)?;
        Ok(row.and_then(|m| m.sync_cursor))
    }

    async fn update_sync_cursor(
        &self,
        id: PlaidItemId,
        cursor: &str,
        uow: Option<&dyn UnitOfWork>,
    ) -> Result<(), RepositoryError> {
        // Targeted update of just sync_cursor + last_synced_at (SPEC §6), rather
        // than a full-row upsert: the rest of the item is untouched on a sync.
        let cursor = cursor.to_owned();
        let now = Utc::now();
        with_conn(&self.db, uow, move |conn| {
            Box::pin(async move {
                let active = plaid_items::ActiveModel {
                    id: Set(id.value()),
                    sync_cursor: Set(Some(cursor)),
                    last_synced_at: Set(Some(now.into())),
                    ..Default::default()
                };
                plaid_items::Entity::update(active)
                    .filter(plaid_items::Column::Id.eq(id.value()))
                    .exec(conn)
                    .await
                    .map_err(map_db_err)?;
                Ok(())
            })
        })
        .await
    }

    async fn save(
        &self,
        item: &PlaidItem,
        uow: Option<&dyn UnitOfWork>,
    ) -> Result<(), RepositoryError> {
        let active = plaid_items_mapper::domain_to_active_model(item);
        with_conn(&self.db, uow, move |conn| {
            Box::pin(async move { upsert::<plaid_items::Entity, _, _>(conn, active).await })
        })
        .await
    }

    async fn list_accounts(&self, user_id: UserId) -> Result<Vec<Account>, RepositoryError> {
        let models = accounts::Entity::find()
            .filter(accounts::Column::UserId.eq(user_id.value()))
            .order_by_asc(accounts::Column::Name)
            .all(&self.db)
            .await
            .map_err(map_db_err)?;
        models
            .into_iter()
            .map(|m| accounts_mapper::model_to_domain(m).map_err(map_read))
            .collect()
    }

    async fn find_account(&self, id: AccountId) -> Result<Option<Account>, RepositoryError> {
        let model = accounts::Entity::find_by_id(id.value())
            .one(&self.db)
            .await
            .map_err(map_db_err)?;
        model
            .map(accounts_mapper::model_to_domain)
            .transpose()
            .map_err(map_read)
    }

    async fn find_account_by_plaid_id(
        &self,
        plaid_account_id: &str,
    ) -> Result<Option<Account>, RepositoryError> {
        let model = accounts::Entity::find()
            .filter(accounts::Column::PlaidAccountId.eq(plaid_account_id))
            .one(&self.db)
            .await
            .map_err(map_db_err)?;
        model
            .map(accounts_mapper::model_to_domain)
            .transpose()
            .map_err(map_read)
    }

    async fn save_account(
        &self,
        account: &Account,
        uow: Option<&dyn UnitOfWork>,
    ) -> Result<(), RepositoryError> {
        let active = accounts_mapper::domain_to_active_model(account);
        with_conn(&self.db, uow, move |conn| {
            Box::pin(async move { upsert::<accounts::Entity, _, _>(conn, active).await })
        })
        .await
    }
}
