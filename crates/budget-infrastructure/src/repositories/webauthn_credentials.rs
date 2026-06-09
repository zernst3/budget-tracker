//! `SeaORM` [`WebauthnCredentialRepository`] implementation (`REPO-1`).
//!
//! Reads return domain [`WebauthnCredential`] values via the mappers crate
//! (`REPO-2`); the `SeaORM` `Model` never leaves this module. Writes go through
//! the generic upsert and the [`with_conn`] executor so `save` enlists in a unit
//! of work when one is supplied (`REPO-4` / `REPO-6`).

use async_trait::async_trait;
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter};

use budget_domain::RepositoryError;
use budget_domain::auth::{WebauthnCredential, WebauthnCredentialRepository};
use budget_domain::ids::UserId;
use budget_domain::uow::UnitOfWork;

use budget_entities::webauthn_credentials;
use budget_mappers::webauthn_credentials as cred_mapper;

use crate::conn::with_conn;
use crate::error::map_db_err;
use crate::upsert::upsert;

/// `SeaORM`-backed [`WebauthnCredentialRepository`].
pub struct PostgresWebauthnCredentialRepository {
    db: DatabaseConnection,
}

impl PostgresWebauthnCredentialRepository {
    /// Build the repository over a connection pool.
    #[must_use]
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }
}

#[async_trait]
impl WebauthnCredentialRepository for PostgresWebauthnCredentialRepository {
    async fn list_for_user(
        &self,
        user_id: UserId,
    ) -> Result<Vec<WebauthnCredential>, RepositoryError> {
        let models = webauthn_credentials::Entity::find()
            .filter(webauthn_credentials::Column::UserId.eq(user_id.value()))
            .all(&self.db)
            .await
            .map_err(map_db_err)?;
        Ok(models
            .into_iter()
            .map(cred_mapper::model_to_domain)
            .collect())
    }

    async fn find_by_credential_id(
        &self,
        credential_id: &[u8],
    ) -> Result<Option<WebauthnCredential>, RepositoryError> {
        let model = webauthn_credentials::Entity::find()
            .filter(webauthn_credentials::Column::CredentialId.eq(credential_id.to_vec()))
            .one(&self.db)
            .await
            .map_err(map_db_err)?;
        Ok(model.map(cred_mapper::model_to_domain))
    }

    async fn save(
        &self,
        credential: &WebauthnCredential,
        uow: Option<&dyn UnitOfWork>,
    ) -> Result<(), RepositoryError> {
        let active = cred_mapper::domain_to_active_model(credential);
        with_conn(&self.db, uow, move |conn| {
            Box::pin(
                async move { upsert::<webauthn_credentials::Entity, _, _>(conn, active).await },
            )
        })
        .await
    }
}
