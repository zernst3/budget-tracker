//! Generic insert-or-update (upsert) helper for `save`-style repository methods.
//!
//! Every aggregate's `save` is "insert the row, or update it if its primary key
//! already exists." Expressed once here as an `ON CONFLICT (pk) DO UPDATE` over
//! all non-primary-key columns, so each repository's `save` is one statement and
//! the upsert semantics cannot drift per aggregate (robustness-over-terseness:
//! one audited primitive beats ten hand-written upserts).
//!
//! The conflict target is the entity's primary key; the update set is every
//! column except the primary key (the PK is the conflict key, never updated).
//! Columns are enumerated via `Iterable`, so adding a column to an entity is
//! automatically covered with no change here.

use sea_orm::sea_query::OnConflict;
use sea_orm::{
    ActiveModelTrait, ConnectionTrait, EntityTrait, IdenStatic, IntoActiveModel, Iterable,
    PrimaryKeyToColumn,
};

use budget_domain::RepositoryError;

use crate::error::map_db_err;

/// Upsert one active model: insert it, or update every non-PK column on PK
/// conflict.
///
/// `E` is the entity; `A` its active model. The conflict target is `E`'s primary
/// key; the update columns are all of `E`'s columns minus the primary-key
/// column(s). Runs against any [`ConnectionTrait`] (pool or enlisted
/// transaction), so callers route it through [`crate::conn::with_conn`].
///
/// # Errors
/// Translates any `SeaORM` `DbErr` into a [`RepositoryError`] via
/// [`map_db_err`].
pub(crate) async fn upsert<E, A, C>(conn: &C, model: A) -> Result<(), RepositoryError>
where
    E: EntityTrait,
    E::Model: IntoActiveModel<A>,
    A: ActiveModelTrait<Entity = E> + Send,
    C: ConnectionTrait,
{
    // Primary-key columns (the conflict target, never in the update set).
    let pk_columns: Vec<E::Column> = E::PrimaryKey::iter()
        .map(PrimaryKeyToColumn::into_column)
        .collect();

    // Every non-PK column is updated on conflict. `Column` does not implement
    // `PartialEq` generically, so compare by the column's static identifier
    // (`IdenStatic::as_str`) to exclude the primary key from the update set.
    let update_columns: Vec<E::Column> = E::Column::iter()
        .filter(|col| !pk_columns.iter().any(|pk| pk.as_str() == col.as_str()))
        .collect();

    let mut on_conflict = OnConflict::columns(pk_columns);
    on_conflict.update_columns(update_columns);

    E::insert(model)
        .on_conflict(on_conflict)
        .exec_without_returning(conn)
        .await
        .map_err(map_db_err)?;

    Ok(())
}
