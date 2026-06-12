//! Manual (user-entered) portfolio data sources — the `Position` and
//! `CashBalance` persistence adapters for the AI Portfolio Insights feature
//! (`REPO-1`, `docs/AI_FEATURE_DESIGN.md §Phase 2`).
//!
//! "Manual" names the data PROVENANCE: these rows are entered by the user through
//! the positions UI, as opposed to a future Plaid-backed source (`§Phase 7`,
//! behind the same ports). The adapters are ordinary `SeaORM`-backed repositories
//! that satisfy the read ports ([`PositionSource`]/[`CashBalanceSource`]) the
//! review use-case grounds against AND the write ports
//! ([`PositionRepository`]/[`CashBalanceRepository`]) the UI mutates through.
//!
//! ## Why [`ManualCashBalanceSource`] is bound to a [`UserId`]
//!
//! The locked [`CashBalanceRepository::upsert`] surface takes only
//! `&CashBalance`, and the domain [`CashBalance`] value (a thin
//! `account_label`/`balance`/`reserved` triple, `BUDGET-CASH-1`) carries no
//! `user_id`. In single-user V1 (`SPEC §9`: one provisioned user, no signup) the
//! owning user is a wiring-time constant, so this adapter is constructed bound to
//! that `UserId` and supplies it on every write. A balance is keyed by
//! `(user_id, account_label)`, so `upsert` resolves any existing row's id by that
//! natural key (reusing it to update in place) before delegating to the generic
//! PK-conflict [`upsert`](crate::upsert::upsert).
//!
//! [`Position`] needs no such binding: the domain [`Position`] carries its own
//! `user_id`, and `delete` is `user_id`-scoped (`SPEC §9.1` defense in depth).

use async_trait::async_trait;
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter};

use budget_domain::RepositoryError;
use budget_domain::ids::{PositionId, UserId};
use budget_domain::portfolio::{CashBalance, CashBalanceSource, Position, PositionSource};
use budget_domain::repositories::{CashBalanceRepository, PositionRepository};

use budget_entities::{cash_balances, positions};
use budget_mappers::{cash_balances as cash_balances_mapper, positions as positions_mapper};

use crate::repositories::map_read;

// ===========================================================================
// ManualPositionSource
// ===========================================================================

/// `SeaORM`-backed manual [`PositionRepository`] (and thus [`PositionSource`]).
///
/// The write surface the manual positions UI mutates through; the read surface
/// the review use-case grounds against. Every read/write is scoped to the
/// owning `user_id` (the position carries it; `delete` takes it explicitly,
/// `SPEC §9.1`).
pub struct ManualPositionSource {
    db: DatabaseConnection,
}

impl ManualPositionSource {
    /// Build the source over a connection pool.
    #[must_use]
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }
}

#[async_trait]
impl PositionSource for ManualPositionSource {
    async fn positions_for_user(&self, user_id: UserId) -> Result<Vec<Position>, RepositoryError> {
        let models = positions::Entity::find()
            .filter(positions::Column::UserId.eq(user_id.value()))
            .all(&self.db)
            .await
            .map_err(crate::error::map_db_err)?;
        models
            .into_iter()
            .map(|m| positions_mapper::model_to_domain(m).map_err(map_read))
            .collect()
    }
}

#[async_trait]
impl PositionRepository for ManualPositionSource {
    async fn insert(&self, position: &Position) -> Result<(), RepositoryError> {
        let active = positions_mapper::domain_to_active_model(position);
        positions::Entity::insert(active)
            .exec_without_returning(&self.db)
            .await
            .map_err(crate::error::map_db_err)?;
        Ok(())
    }

    async fn update(&self, position: &Position) -> Result<(), RepositoryError> {
        let active = positions_mapper::domain_to_active_model(position);
        // Update-by-PK; the active model carries the full row (every column Set),
        // so this overwrites the existing row identified by `id`.
        positions::Entity::update(active)
            .filter(positions::Column::UserId.eq(position.user_id.value()))
            .exec(&self.db)
            .await
            .map_err(crate::error::map_db_err)?;
        Ok(())
    }

    async fn delete(&self, user_id: UserId, id: PositionId) -> Result<(), RepositoryError> {
        positions::Entity::delete_many()
            .filter(positions::Column::Id.eq(id.value()))
            .filter(positions::Column::UserId.eq(user_id.value()))
            .exec(&self.db)
            .await
            .map_err(crate::error::map_db_err)?;
        Ok(())
    }
}

// ===========================================================================
// ManualCashBalanceSource
// ===========================================================================

/// `SeaORM`-backed manual [`CashBalanceRepository`] (and thus
/// [`CashBalanceSource`]), bound to the owning [`UserId`].
///
/// Bound to a `UserId` because the locked `upsert(&CashBalance)` surface carries
/// no user scope and the domain [`CashBalance`] is `user_id`-free
/// (`BUDGET-CASH-1`); in single-user V1 (`SPEC §9`) the owner is a wiring-time
/// constant. A balance is keyed by `(user_id, account_label)`.
pub struct ManualCashBalanceSource {
    db: DatabaseConnection,
    user_id: UserId,
}

impl ManualCashBalanceSource {
    /// Build the source over a connection pool, bound to the owning user.
    #[must_use]
    pub fn new(db: DatabaseConnection, user_id: UserId) -> Self {
        Self { db, user_id }
    }
}

#[async_trait]
impl CashBalanceSource for ManualCashBalanceSource {
    async fn balances_for_user(
        &self,
        user_id: UserId,
    ) -> Result<Vec<CashBalance>, RepositoryError> {
        let models = cash_balances::Entity::find()
            .filter(cash_balances::Column::UserId.eq(user_id.value()))
            .all(&self.db)
            .await
            .map_err(crate::error::map_db_err)?;
        models
            .into_iter()
            .map(|m| cash_balances_mapper::model_to_domain(m).map_err(map_read))
            .collect()
    }
}

#[async_trait]
impl CashBalanceRepository for ManualCashBalanceSource {
    async fn upsert(&self, balance: &CashBalance) -> Result<(), RepositoryError> {
        // Resolve the natural-key row id: a balance is keyed by
        // `(user_id, account_label)`. Reuse an existing row's id so the upsert
        // updates in place; otherwise mint a fresh id for the insert.
        let existing = cash_balances::Entity::find()
            .filter(cash_balances::Column::UserId.eq(self.user_id.value()))
            .filter(cash_balances::Column::AccountLabel.eq(balance.account_label.clone()))
            .one(&self.db)
            .await
            .map_err(crate::error::map_db_err)?;
        let id = existing.map_or_else(uuid::Uuid::new_v4, |m| m.id);

        let active =
            cash_balances_mapper::to_active_model(id, self.user_id, balance, chrono::Utc::now());
        crate::upsert::upsert::<cash_balances::Entity, _, _>(&self.db, active).await
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use chrono::{TimeZone, Utc};
    use rust_decimal::Decimal;
    use sea_orm::{DbBackend, MockDatabase, MockExecResult};
    use uuid::Uuid;

    use budget_domain::enums::AccountType;
    use budget_domain::money::Money;
    use budget_domain::portfolio::Ticker;

    fn fixed_now() -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 6, 11, 0, 0, 0).unwrap()
    }

    fn sample_position_model(user_id: Uuid) -> positions::Model {
        positions::Model {
            id: Uuid::new_v4(),
            user_id,
            ticker: "AAPL".to_owned(),
            account_label: "Fidelity Roth".to_owned(),
            account_type: budget_entities::accounts::AccountType::Investment,
            shares: Decimal::new(10, 0),
            cost_basis: Some(Decimal::new(150_000, 2)),
            drip_enabled: false,
            baseline_as_of: fixed_now().into(),
            created_at: fixed_now().into(),
            updated_at: fixed_now().into(),
        }
    }

    fn sample_domain_position() -> Position {
        Position {
            id: PositionId::generate(),
            user_id: UserId::generate(),
            ticker: Ticker::try_new("AAPL").unwrap(),
            account_label: "Fidelity Roth".to_owned(),
            account_type: AccountType::Investment,
            shares: Decimal::new(10, 0),
            cost_basis: Some(Money::from_minor(150_000)),
            drip_enabled: false,
            baseline_as_of: fixed_now(),
            created_at: fixed_now(),
            updated_at: fixed_now(),
        }
    }

    #[tokio::test]
    async fn positions_for_user_maps_rows_to_domain() {
        let user_id = UserId::generate();
        let model = sample_position_model(user_id.value());
        let db = MockDatabase::new(DbBackend::Postgres)
            .append_query_results([[model]])
            .into_connection();
        let repo = ManualPositionSource::new(db);

        let out = repo.positions_for_user(user_id).await.unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].ticker.as_str(), "AAPL");
        assert_eq!(out[0].account_type, AccountType::Investment);
        assert_eq!(out[0].cost_basis, Some(Money::from_minor(150_000)));
    }

    #[tokio::test]
    async fn insert_position_issues_exec_without_error() {
        let db = MockDatabase::new(DbBackend::Postgres)
            .append_exec_results([MockExecResult {
                last_insert_id: 0,
                rows_affected: 1,
            }])
            .into_connection();
        let repo = ManualPositionSource::new(db);
        assert!(repo.insert(&sample_domain_position()).await.is_ok());
    }

    #[tokio::test]
    async fn update_position_issues_exec_without_error() {
        let pos = sample_domain_position();
        // update() does a SELECT-then-UPDATE under SeaORM's update-by-pk; queue a
        // returning row then the exec.
        let model = positions::Model {
            id: pos.id.value(),
            user_id: pos.user_id.value(),
            ticker: "AAPL".to_owned(),
            account_label: "Fidelity Roth".to_owned(),
            account_type: budget_entities::accounts::AccountType::Investment,
            shares: Decimal::new(10, 0),
            cost_basis: Some(Decimal::new(150_000, 2)),
            drip_enabled: false,
            baseline_as_of: fixed_now().into(),
            created_at: fixed_now().into(),
            updated_at: fixed_now().into(),
        };
        let db = MockDatabase::new(DbBackend::Postgres)
            .append_query_results([[model]])
            .into_connection();
        let repo = ManualPositionSource::new(db);
        assert!(repo.update(&pos).await.is_ok());
    }

    #[tokio::test]
    async fn delete_position_issues_exec_without_error() {
        let db = MockDatabase::new(DbBackend::Postgres)
            .append_exec_results([MockExecResult {
                last_insert_id: 0,
                rows_affected: 1,
            }])
            .into_connection();
        let repo = ManualPositionSource::new(db);
        let res = repo
            .delete(UserId::generate(), PositionId::generate())
            .await;
        assert!(res.is_ok());
    }

    #[tokio::test]
    async fn balances_for_user_maps_rows_to_domain() {
        let user_id = UserId::generate();
        let model = cash_balances::Model {
            id: Uuid::new_v4(),
            user_id: user_id.value(),
            account_label: "Emergency Fund".to_owned(),
            balance: Decimal::new(500_000, 2),
            reserved: true,
            created_at: fixed_now().into(),
            updated_at: fixed_now().into(),
        };
        let db = MockDatabase::new(DbBackend::Postgres)
            .append_query_results([[model]])
            .into_connection();
        let repo = ManualCashBalanceSource::new(db, user_id);

        let out = repo.balances_for_user(user_id).await.unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].account_label, "Emergency Fund");
        assert_eq!(out[0].balance, Money::from_minor(500_000));
        assert!(out[0].reserved);
    }

    #[tokio::test]
    async fn upsert_balance_reuses_existing_row_id() {
        let user_id = UserId::generate();
        let existing_id = Uuid::new_v4();
        let existing = cash_balances::Model {
            id: existing_id,
            user_id: user_id.value(),
            account_label: "Checking".to_owned(),
            balance: Decimal::new(100_000, 2),
            reserved: false,
            created_at: fixed_now().into(),
            updated_at: fixed_now().into(),
        };
        let db = MockDatabase::new(DbBackend::Postgres)
            // The natural-key lookup returns the existing row,
            .append_query_results([[existing]])
            // then the ON CONFLICT upsert exec.
            .append_exec_results([MockExecResult {
                last_insert_id: 0,
                rows_affected: 1,
            }])
            .into_connection();
        let repo = ManualCashBalanceSource::new(db, user_id);

        let balance = CashBalance {
            account_label: "Checking".to_owned(),
            balance: Money::from_minor(123_456),
            reserved: false,
        };
        assert!(repo.upsert(&balance).await.is_ok());
    }

    #[tokio::test]
    async fn upsert_balance_mints_id_when_absent() {
        let user_id = UserId::generate();
        let db = MockDatabase::new(DbBackend::Postgres)
            // No existing row for the natural key,
            .append_query_results([Vec::<cash_balances::Model>::new()])
            // then the ON CONFLICT upsert exec inserts a fresh row.
            .append_exec_results([MockExecResult {
                last_insert_id: 0,
                rows_affected: 1,
            }])
            .into_connection();
        let repo = ManualCashBalanceSource::new(db, user_id);

        let balance = CashBalance {
            account_label: "Brokerage Cash".to_owned(),
            balance: Money::from_minor(7_500),
            reserved: false,
        };
        assert!(repo.upsert(&balance).await.is_ok());
    }
}
