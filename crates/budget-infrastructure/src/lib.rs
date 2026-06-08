//! Concrete adapters for the budget tracker.
//!
//! `SeaORM` repository implementations + the `SeaOrmUow` / `SeaOrmUowProvider`
//! unit-of-work primitive (`REPO-*`), the Azure Key Vault client (Plaid token
//! stored only as a secret reference, `BUDGET-PLAID-TOKEN-VAULT-1`), and the
//! Plaid Transactions-only client. Depends on domain, entities, and mappers.
//!
//! Adapters land in build step 3+ (see `.build-progress.md`).
//!
//! The schema migration runner is wired now: [`run_pending_migrations`] applies
//! the `budget-migration` `Migrator` so the SPEC §5 tables and §12 DB
//! constraints exist before any repository runs.

use sea_orm_migration::MigratorTrait;

/// Run every pending schema migration against the connected database.
///
/// Call this once at server startup, after the connection pool is established
/// and before serving traffic, so the schema (`SPEC` §5 tables + §12 DB
/// constraints) is materialized before any repository runs a query.
///
/// Idempotent (`PROC-CI-MIGRATION-HYGIENE-1`): `sea-orm-migration` records
/// applied migrations in its `seaql_migrations` journal, so already-applied
/// steps are skipped. `None` applies all pending migrations.
///
/// # Errors
///
/// Returns the underlying [`sea_orm::DbErr`] if a migration fails to apply
/// (connection loss, a DDL statement error, or a journal-write failure).
pub async fn run_pending_migrations(
    db: &sea_orm::DatabaseConnection,
) -> Result<(), sea_orm::DbErr> {
    budget_migration::Migrator::up(db, None).await
}
