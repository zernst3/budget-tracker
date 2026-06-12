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
use budget_domain::portfolio::{
    CashBalance, CashBalanceSource, Position, PositionSource, UploadedPosition,
};
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

    async fn set_drip_enabled(
        &self,
        user_id: UserId,
        id: PositionId,
        enabled: bool,
    ) -> Result<(), RepositoryError> {
        use sea_orm::sea_query::Expr;
        // Targeted single-column update: set ONLY `drip_enabled` (+ `updated_at`),
        // user_id-scoped (SPEC §9.1). Leaves `shares`/`baseline_as_of` untouched so
        // the toggle is per-position config that survives uploads (§2.7).
        positions::Entity::update_many()
            .col_expr(positions::Column::DripEnabled, Expr::value(enabled))
            .col_expr(
                positions::Column::UpdatedAt,
                Expr::value(chrono::Utc::now()),
            )
            .filter(positions::Column::Id.eq(id.value()))
            .filter(positions::Column::UserId.eq(user_id.value()))
            .exec(&self.db)
            .await
            .map_err(crate::error::map_db_err)?;
        Ok(())
    }

    async fn upsert_account(
        &self,
        user_id: UserId,
        account_label: &str,
        account_type: budget_domain::enums::AccountType,
        uploaded: &[UploadedPosition],
        baseline: chrono::DateTime<chrono::Utc>,
    ) -> Result<(), RepositoryError> {
        use sea_orm::TransactionTrait;

        // Snapshot the inputs into owned values so the transaction closure (which
        // must be `'static + Send`) can capture them.
        let user_uuid = user_id.value();
        let label = account_label.to_owned();
        let uploaded: Vec<UploadedPosition> = uploaded.to_vec();

        // The whole per-account reconcile runs in ONE transaction so a partial
        // upload can never leave the account half-reconciled
        // (RUST-SEAORM-INTRA-AGGREGATE-TX-1, ARCH-EXPLICIT-TX-1). On any error the
        // transaction rolls back.
        self.db
            .transaction::<_, (), sea_orm::DbErr>(move |tx| {
                Box::pin(async move {
                    // 1. Load the existing positions IN THIS ACCOUNT ONLY. Every
                    // read + write below is filtered to `(user_id, account_label)`,
                    // so positions in other accounts are never observed or touched.
                    let existing_models = positions::Entity::find()
                        .filter(positions::Column::UserId.eq(user_uuid))
                        .filter(positions::Column::AccountLabel.eq(label.clone()))
                        .all(tx)
                        .await?;
                    let existing: Vec<Position> = existing_models
                        .into_iter()
                        .map(|m| {
                            let ticker = m.ticker.clone();
                            positions_mapper::model_to_domain(m).map_err(|e| {
                                sea_orm::DbErr::Custom(format!(
                                    "corrupt stored position '{ticker}': {e}"
                                ))
                            })
                        })
                        .collect::<Result<_, _>>()?;

                    // 2. The PURE reconcile decision (account-scoped removal sweep +
                    // re-baseline-preserving-drip + new-insert-DRIP-off). Unit-tested
                    // directly below; the transaction only EXECUTES the plan.
                    let plan = plan_account_upsert(
                        &existing,
                        user_id,
                        &label,
                        account_type,
                        &uploaded,
                        baseline,
                    );

                    // 3a. Removal sweep — every id here came from the account-scoped
                    // `existing`, so the sweep can never reach another account (§2.7).
                    for id in plan.deletes {
                        positions::Entity::delete_by_id(id.value()).exec(tx).await?;
                    }
                    // 3b. Updates (survivors, drip_enabled preserved) + inserts (new,
                    // DRIP-off), each through the single `domain_to_active_model`
                    // mapper (RUST-MAPPER-1).
                    for position in plan.updates {
                        let active = positions_mapper::domain_to_active_model(&position);
                        positions::Entity::update(active).exec(tx).await?;
                    }
                    for position in plan.inserts {
                        let active = positions_mapper::domain_to_active_model(&position);
                        positions::Entity::insert(active)
                            .exec_without_returning(tx)
                            .await?;
                    }
                    Ok(())
                })
            })
            .await
            .map_err(|e| crate::error::map_db_err(map_tx_err(e)))?;
        Ok(())
    }
}

/// The pure reconcile decision for a per-account upload upsert
/// (`docs/DRIP_REALTIME_DESIGN.md §2.7/§6`). Separated from the I/O so the
/// account-scoping + `drip_enabled`-preservation invariants are unit-testable
/// without a database.
///
/// `existing` MUST already be scoped to the single uploaded account
/// (`(user_id, account_label)`); the planner never reaches beyond it. Returns the
/// ids to delete (sold-off survivors), the survivor rows to update (re-baselined,
/// `drip_enabled` preserved), and the new rows to insert (`drip_enabled = false`).
struct AccountUpsertPlan {
    deletes: Vec<PositionId>,
    updates: Vec<Position>,
    inserts: Vec<Position>,
}

fn plan_account_upsert(
    existing: &[Position],
    user_id: UserId,
    account_label: &str,
    account_type: budget_domain::enums::AccountType,
    uploaded: &[UploadedPosition],
    baseline: chrono::DateTime<chrono::Utc>,
) -> AccountUpsertPlan {
    use std::collections::HashSet;

    let uploaded_tickers: HashSet<&str> = uploaded.iter().map(|u| u.ticker.as_str()).collect();

    // Removal sweep: existing rows whose ticker is absent from the upload are sold
    // off. `existing` is already account-scoped, so this never touches another
    // account.
    let deletes: Vec<PositionId> = existing
        .iter()
        .filter(|p| !uploaded_tickers.contains(p.ticker.as_str()))
        .map(|p| p.id)
        .collect();

    let mut updates = Vec::new();
    let mut inserts = Vec::new();
    for u in uploaded {
        match existing.iter().find(|p| p.ticker == u.ticker) {
            // Surviving position: re-baseline. PRESERVE `id`, `created_at`,
            // `account_type`, and `drip_enabled` (per-position config persists,
            // §2.7); only `shares`/`cost_basis`/`baseline_as_of` move. The DRIP
            // estimate resets because current shares derive from applications with
            // `pay_date > baseline_as_of` (§6).
            Some(prior) => updates.push(Position {
                shares: u.shares,
                cost_basis: u.cost_basis,
                baseline_as_of: baseline,
                updated_at: baseline,
                ..prior.clone()
            }),
            // New holding in this account: insert DRIP-off (opt-in, §2.7).
            None => inserts.push(Position {
                id: PositionId::new(uuid::Uuid::new_v4()),
                user_id,
                ticker: u.ticker.clone(),
                account_label: account_label.to_owned(),
                account_type,
                shares: u.shares,
                cost_basis: u.cost_basis,
                drip_enabled: false,
                baseline_as_of: baseline,
                created_at: baseline,
                updated_at: baseline,
            }),
        }
    }

    AccountUpsertPlan {
        deletes,
        updates,
        inserts,
    }
}

/// Flatten a `SeaORM` [`TransactionError`] into a plain [`DbErr`] so the existing
/// [`map_db_err`](crate::error::map_db_err) boundary can translate it.
fn map_tx_err(e: sea_orm::TransactionError<sea_orm::DbErr>) -> sea_orm::DbErr {
    match e {
        sea_orm::TransactionError::Connection(db) | sea_orm::TransactionError::Transaction(db) => {
            db
        }
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

    // -- Per-account upsert planner (§2.7/§6, ORCH-NEW-PATH-TESTS-1) ----------

    /// A domain position in an explicit account, with an explicit ticker + drip.
    fn pos_in(ticker: &str, account: &str, shares: i64, drip: bool) -> Position {
        Position {
            id: PositionId::generate(),
            user_id: UserId::generate(),
            ticker: Ticker::try_new(ticker).unwrap(),
            account_label: account.to_owned(),
            account_type: AccountType::Investment,
            shares: Decimal::new(shares, 0),
            cost_basis: None,
            drip_enabled: drip,
            baseline_as_of: fixed_now(),
            created_at: fixed_now(),
            updated_at: fixed_now(),
        }
    }

    fn uploaded(ticker: &str, shares: i64) -> UploadedPosition {
        UploadedPosition {
            ticker: Ticker::try_new(ticker).unwrap(),
            shares: Decimal::new(shares, 0),
            cost_basis: None,
        }
    }

    #[test]
    fn plan_preserves_drip_on_a_survivor_and_rebaselines_it() {
        // Existing Brokerage AAPL (DRIP ON), 10 shares. Upload re-confirms AAPL at
        // 12 shares: the survivor is UPDATED, drip_enabled PRESERVED, shares +
        // baseline moved.
        let user = UserId::generate();
        let later = Utc.with_ymd_and_hms(2026, 7, 1, 0, 0, 0).unwrap();
        let existing = vec![pos_in("AAPL", "Brokerage", 10, true)];
        let plan = plan_account_upsert(
            &existing,
            user,
            "Brokerage",
            AccountType::Investment,
            &[uploaded("AAPL", 12)],
            later,
        );
        assert!(plan.deletes.is_empty());
        assert!(plan.inserts.is_empty());
        assert_eq!(plan.updates.len(), 1);
        let updated = &plan.updates[0];
        assert!(
            updated.drip_enabled,
            "drip_enabled is PRESERVED across upload"
        );
        assert_eq!(updated.shares, Decimal::new(12, 0), "re-baselined shares");
        assert_eq!(
            updated.baseline_as_of, later,
            "baseline moved to upload date"
        );
        assert_eq!(
            updated.id, existing[0].id,
            "row identity preserved (update)"
        );
    }

    #[test]
    fn plan_removes_sold_off_and_inserts_new_drip_off() {
        // Existing Brokerage {AAPL, MSFT}. Upload {AAPL, NVDA}: MSFT sold off
        // (delete), AAPL survives (update), NVDA new (insert DRIP-off).
        let user = UserId::generate();
        let aapl = pos_in("AAPL", "Brokerage", 10, true);
        let msft = pos_in("MSFT", "Brokerage", 5, false);
        let existing = vec![aapl.clone(), msft.clone()];
        let plan = plan_account_upsert(
            &existing,
            user,
            "Brokerage",
            AccountType::Investment,
            &[uploaded("AAPL", 10), uploaded("NVDA", 3)],
            fixed_now(),
        );
        assert_eq!(plan.deletes, vec![msft.id], "MSFT (absent) is swept");
        assert_eq!(plan.updates.len(), 1);
        assert_eq!(plan.updates[0].ticker.as_str(), "AAPL");
        assert_eq!(plan.inserts.len(), 1);
        let nvda = &plan.inserts[0];
        assert_eq!(nvda.ticker.as_str(), "NVDA");
        assert!(
            !nvda.drip_enabled,
            "a new holding is DRIP-off by default (§2.7)"
        );
        assert_eq!(nvda.account_label, "Brokerage");
    }

    #[test]
    fn plan_only_operates_on_the_account_scoped_existing_rows() {
        // The repo passes `existing` already filtered to the uploaded account, so
        // the planner — and therefore the deletes — can ONLY reference that
        // account. Feeding ONLY account-A rows, uploading account A with an empty
        // payload sweeps exactly account A's rows and produces no insert/update.
        // (Cross-account isolation is additionally proven end-to-end by the
        // DATABASE_URL-gated live test.)
        let user = UserId::generate();
        let a1 = pos_in("AAPL", "Brokerage", 10, true);
        let a2 = pos_in("MSFT", "Brokerage", 5, false);
        let existing = vec![a1.clone(), a2.clone()];
        let plan = plan_account_upsert(
            &existing,
            user,
            "Brokerage",
            AccountType::Investment,
            &[],
            fixed_now(),
        );
        // Both Brokerage rows swept; nothing else.
        assert_eq!(plan.deletes.len(), 2);
        assert!(plan.deletes.contains(&a1.id));
        assert!(plan.deletes.contains(&a2.id));
        assert!(plan.updates.is_empty());
        assert!(plan.inserts.is_empty());
    }
}
