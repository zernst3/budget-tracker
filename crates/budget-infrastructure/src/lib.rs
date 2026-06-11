//! Concrete adapters for the budget tracker.
//!
//! `SeaORM` repository implementations + the `SeaOrmUow` / `SeaOrmUowProvider`
//! unit-of-work primitive (`REPO-*`), the Azure Key Vault client (Plaid token
//! stored only as a secret reference, `BUDGET-PLAID-TOKEN-VAULT-1`), and the
//! Plaid Transactions-only client. Depends on domain, entities, and mappers.
//!
//! Build step 3 lands the repository layer: one `Postgres*Repository` per
//! aggregate trait, the [`SeaOrmUow`](uow::SeaOrmUow) /
//! [`SeaOrmUowProvider`](uow::SeaOrmUowProvider) unit-of-work primitive, the
//! shared error translation, the executor resolver, and the generic upsert.
//!
//! The schema migration runner is also wired: [`run_pending_migrations`] applies
//! the `budget-migration` `Migrator` so the SPEC §5 tables and §12 DB
//! constraints exist before any repository runs.
//!
//! Build step 7 adds the [`auth`] subsystem (`BUDGET-AUTH-GATE-1`, `SPEC §9.1`):
//! the Argon2id password hasher, the RFC 6238 TOTP engine (`AUTH-1/2`), the
//! `webauthn-rs` passkey engine, the Postgres-backed session store + secure
//! cookie policy, the [`AuthedUser`](auth::AuthedUser) enforce-by-construction
//! gate, the Azure Key Vault secret-vault client
//! (`BUDGET-PLAID-TOKEN-VAULT-1`), and the `webauthn_credentials` repository.

pub mod advisor;
pub mod auth;
pub mod conn;
pub mod error;
pub mod market_data;
pub mod plaid;
pub mod portfolio_sources;
pub mod repositories;
pub mod uow;
pub mod upsert;

// Re-export the concrete repository impls + the unit-of-work primitive at the
// crate root so the application edge wires them without deep paths.
pub use repositories::budgets::PostgresBudgetRepository;
pub use repositories::funds::PostgresFundRepository;
pub use repositories::months::PostgresMonthRepository;
pub use repositories::paycheck_config::PostgresPaycheckConfigRepository;
pub use repositories::plaid_items::PostgresPlaidItemRepository;
pub use repositories::review_runs::PostgresReviewRunRepository;
pub use repositories::transactions::PostgresTransactionRepository;
pub use repositories::users::PostgresUserRepository;
pub use repositories::webauthn_credentials::PostgresWebauthnCredentialRepository;
pub use uow::{SeaOrmUow, SeaOrmUowProvider};

// Manual (user-entered) portfolio sources for AI Portfolio Insights
// (`docs/AI_FEATURE_DESIGN.md §Phase 2`): the `Position`/`CashBalance`
// persistence adapters the positions UI mutates through and the review use-case
// grounds against.
pub use portfolio_sources::{ManualCashBalanceSource, ManualPositionSource};

// Market-data adapters for AI Portfolio Insights (`§Phase 3`): the
// fixture-configured `MockMarketDataProvider` (the real HTTP adapter is an Open
// Item, returns `Err` until a provider is confirmed).
pub use market_data::{MockMarketDataProvider, MockQuote};

// Investment-advisor adapters for AI Portfolio Insights (`§Phase 4`): the
// fixture-driven `MockInvestmentAdvisor` parses captured Gemini JSON through the
// same wire->domain path the real (Phase-6) adapter will. The Gemini wire DTOs +
// `parse_advisor_response` stay `pub(crate)` (§0.3); the real `GeminiAdvisor` is
// Phase 6.
pub use advisor::{MOCK_MODEL_ID, MockInvestmentAdvisor, MockMode};

// Plaid integration (build step 8, SPEC §6): the reqwest HTTP client + the
// cursor-sync/reconcile engine. Both implement domain ports so the service layer
// + tests work against abstractions (no live Plaid call in tests).
pub use plaid::{HttpPlaidApi, PlaidCredentials, PlaidEnvironment, SeaOrmPlaidSyncEngine};

// STAGE-1 local-testing adapters (OFF by default; selected only by the explicit
// `PLAID_MODE=mock` opt-in at the budget-ui `server_state` wiring seam). The real
// `HttpPlaidApi` + `AzureKeyVault` remain the default/production path.
pub use plaid::{InMemorySecretVault, MOCK_ACCESS_TOKEN, MockPlaidApi};

// Auth subsystem (build step 7, BUDGET-AUTH-GATE-1): the concrete adapters of
// the domain auth ports, the session store + cookie policy, and the AuthedUser
// gate. The HTTP host that mounts the gate is the frontend phase.
pub use auth::webauthn::{AuthenticationOutcome, RegisteredPasskey};
pub use auth::{
    Argon2idHasher, AuthState, AuthedUser, AzureKeyVault, Rfc6238TotpService, SessionLayerConfig,
    WebauthnService, build_session_layer,
};

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
