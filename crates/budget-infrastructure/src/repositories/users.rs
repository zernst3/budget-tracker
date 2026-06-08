//! `SeaORM` [`UserRepository`] implementation (`REPO-1`).
//!
//! Reads return domain [`User`] values via the mappers crate (`REPO-2`); the
//! `SeaORM` `Model` never leaves this module. Writes go through the generic
//! upsert and the [`with_conn`] executor so `save` enlists in a unit of work when
//! one is supplied (`REPO-4` / `REPO-6`).

use async_trait::async_trait;
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter};

use budget_domain::RepositoryError;
use budget_domain::ids::UserId;
use budget_domain::repositories::UserRepository;
use budget_domain::uow::UnitOfWork;
use budget_domain::user::User;

use budget_entities::users;
use budget_mappers::users as users_mapper;

use crate::conn::with_conn;
use crate::error::map_db_err;
use crate::repositories::map_read;
use crate::upsert::upsert;

/// `SeaORM`-backed [`UserRepository`].
pub struct PostgresUserRepository {
    db: DatabaseConnection,
}

impl PostgresUserRepository {
    /// Build the repository over a connection pool.
    #[must_use]
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }
}

#[async_trait]
impl UserRepository for PostgresUserRepository {
    async fn find_by_id(&self, id: UserId) -> Result<Option<User>, RepositoryError> {
        let model = users::Entity::find_by_id(id.value())
            .one(&self.db)
            .await
            .map_err(map_db_err)?;
        model
            .map(users_mapper::model_to_domain)
            .transpose()
            .map_err(map_read)
    }

    async fn find_by_email(&self, email: &str) -> Result<Option<User>, RepositoryError> {
        let model = users::Entity::find()
            .filter(users::Column::Email.eq(email))
            .one(&self.db)
            .await
            .map_err(map_db_err)?;
        model
            .map(users_mapper::model_to_domain)
            .transpose()
            .map_err(map_read)
    }

    async fn save(&self, user: &User, uow: Option<&dyn UnitOfWork>) -> Result<(), RepositoryError> {
        let active = users_mapper::domain_to_active_model(user);
        with_conn(&self.db, uow, move |conn| {
            Box::pin(async move { upsert::<users::Entity, _, _>(conn, active).await })
        })
        .await
    }
}
