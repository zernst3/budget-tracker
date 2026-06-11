//! Server-side application state for the Dioxus server functions
//! (`BUDGET-AUTH-GATE-1`, `SPEC §9.1`, `D1`).
//!
//! This module is **server-only** (`#[cfg(feature = "server")]`): it pulls the
//! `budget-app-services` use-case layer and the `budget-infrastructure` repos +
//! auth adapters, none of which may enter the wasm32 client graph. It is the one
//! place the concrete persistence + auth collaborators are wired together for the
//! server-function entry layer.
//!
//! ## What it holds
//!
//! - [`AppState::users`] — the [`UserRepository`] the [`AuthedUser`] gate
//!   (`services::gate`) loads the authenticated user through.
//! - [`AppState::auth`] — the [`AuthService`] the login server function calls to
//!   verify password + mandatory TOTP (`SPEC §9.1`).
//! - [`MonthViewState`] — the repos + lifecycle service the B4 month-view server
//!   functions use (`get_month_view`, `ensure_month`). Extracted from the Axum
//!   extension inside each server function via [`MonthViewState::extract`].
//!
//! ## How a server function reaches it
//!
//! The state is mounted on the Axum router as an [`axum::Extension`] layer in
//! `budget-server`'s `main` (so it applies to the server-function routes too).
//! Inside a server function it is pulled back out via
//! [`FullstackContext::extract`] of `Extension<AppState>` — see
//! [`crate::services::gate`], which centralizes that extraction so no server
//! function reaches for the state directly. The state is cheaply cloneable
//! (`Arc` handles), as Axum's `Extension` requires.

use std::sync::Arc;

use budget_app_services::AuthService;
use budget_domain::repositories::UserRepository;
use budget_domain::repositories::{BudgetRepository, MonthRepository, TransactionRepository};
use budget_infrastructure::auth::{Argon2idHasher, Rfc6238TotpService};
use budget_infrastructure::{
    PostgresBudgetRepository, PostgresMonthRepository, PostgresTransactionRepository,
    PostgresUserRepository, PostgresWebauthnCredentialRepository, WebauthnService,
};
use sea_orm::DatabaseConnection;

/// The default relying-party id when `WEBAUTHN_RP_ID` is unset (local dev).
const DEFAULT_RP_ID: &str = "localhost";
/// The default relying-party origin when `WEBAUTHN_RP_ORIGIN` is unset (local
/// dev over the `dx serve` default port).
const DEFAULT_RP_ORIGIN: &str = "http://localhost:8080";
/// The human-readable relying-party name shown by the OS passkey prompt.
const RP_NAME: &str = "Budget Tracker";

/// The server-side state shared with every server function.
///
/// Cloning is cheap: every field is an `Arc` (or `Arc`-backed service), so the
/// state can be stored in an Axum [`Extension`](axum::Extension) layer and pulled
/// into each server-function invocation without deep copies.
#[derive(Clone)]
pub struct AppState {
    /// Loads the authenticated user for the [`AuthedUser`](crate::services::gate)
    /// gate, and scopes data queries to `user_id` (`SPEC §9.1`).
    pub users: Arc<dyn UserRepository>,
    /// Authentication use cases: password (Argon2) + mandatory TOTP verification
    /// establishing a session, TOTP enrollment, passkey persistence
    /// (`BUDGET-AUTH-GATE-1`).
    pub auth: Arc<AuthService>,
    /// The `webauthn-rs` passkey ceremony engine (`SPEC §9.1`: biometric login).
    /// Built once per process from the relying-party config; the passkey
    /// register/authenticate server functions drive the start/finish ceremonies
    /// through it.
    pub webauthn: Arc<WebauthnService>,
}

impl AppState {
    /// Assemble the server state from collaborators.
    ///
    /// Used directly by tests that inject fakes; production code uses
    /// [`AppState::from_connections`].
    #[must_use]
    pub fn new(
        users: Arc<dyn UserRepository>,
        auth: Arc<AuthService>,
        webauthn: Arc<WebauthnService>,
    ) -> Self {
        Self {
            users,
            auth,
            webauthn,
        }
    }

    /// Wire the production state from two live `SeaORM` connections — one for the
    /// user repository, one for the webauthn-credential repository.
    ///
    /// Two connections (rather than one cloned) are taken deliberately: under the
    /// `mock` dev-feature `SeaORM`'s [`DatabaseConnection`] drops `Clone` (the same
    /// caveat the live integration tests handle with a fresh connection), so the
    /// state assembles from independent connections instead of cloning one. The
    /// shared `users` handle is reused for the [`AuthedUser`](crate::services::gate)
    /// gate's lookup (`SPEC §9`: single user, no multi-user code).
    ///
    /// The `webauthn-rs` engine's relying-party id + origin come from the
    /// `WEBAUTHN_RP_ID` / `WEBAUTHN_RP_ORIGIN` environment variables (defaulting to
    /// `localhost` / `http://localhost:8080` for a local `dx serve` run). In
    /// production these MUST be set to the deployed HTTPS origin (`SPEC §9.1`:
    /// passkeys are phishing-resistant precisely because the browser binds the
    /// assertion to this exact origin).
    ///
    /// # Errors
    /// Returns the `webauthn-rs` builder error string if `WEBAUTHN_RP_ORIGIN` is
    /// not a valid URL or the relying-party parameters are rejected.
    pub fn from_connections(
        users_db: DatabaseConnection,
        credentials_db: DatabaseConnection,
    ) -> Result<Self, String> {
        let users: Arc<dyn UserRepository> = Arc::new(PostgresUserRepository::new(users_db));
        let credentials = Arc::new(PostgresWebauthnCredentialRepository::new(credentials_db));
        let passwords = Arc::new(Argon2idHasher::new());
        let totp = Arc::new(Rfc6238TotpService::new());
        let auth = Arc::new(AuthService::new(
            users.clone(),
            credentials,
            passwords,
            totp,
        ));

        let rp_id = std::env::var("WEBAUTHN_RP_ID").unwrap_or_else(|_| DEFAULT_RP_ID.to_owned());
        let rp_origin =
            std::env::var("WEBAUTHN_RP_ORIGIN").unwrap_or_else(|_| DEFAULT_RP_ORIGIN.to_owned());
        let webauthn =
            Arc::new(WebauthnService::new(&rp_id, &rp_origin, RP_NAME).map_err(|e| e.to_string())?);

        Ok(Self {
            users,
            auth,
            webauthn,
        })
    }
}

// ---------------------------------------------------------------------------
// MonthViewState — the additional server state for B4 month-view server fns
// ---------------------------------------------------------------------------

/// Server state used by the B4 month-view server functions (`get_month_view`,
/// `ensure_month`).
///
/// Holds the concrete repos and the lifecycle service the month-view read path
/// needs. Mounted as an Axum `Extension` layer alongside [`AppState`] in
/// `budget-server`'s `main`. Extracted inside each server function by
/// [`MonthViewState::extract`].
///
/// `Arc`-backed throughout so cloning is cheap per Axum's `Extension` contract.
#[derive(Clone)]
pub struct MonthViewState {
    /// Month repository — look up a specific `(year, month)`.
    pub months: Arc<dyn MonthRepository>,
    /// Budget repository — list the categories for a budget version.
    pub budgets: Arc<dyn BudgetRepository>,
    /// Transaction repository — `category_spent_for_month` single-query aggregation.
    pub transactions: Arc<dyn TransactionRepository>,
    /// Fund repository — `list_buffer_financed_transaction_ids` (`SPEC §4.9` D7):
    /// the full-price tracking rows the ledger day-totals must exclude so they match
    /// the month-close net (`BUDGET-NO-DOUBLE-CHARGE-1`).
    pub funds: Arc<dyn budget_domain::repositories::FundRepository>,
    /// Month lifecycle — `ensure_current_month` (lazy-init, idempotent).
    pub lifecycle: Arc<budget_app_services::MonthLifecycleService>,
}

impl MonthViewState {
    /// Assemble from independent live `SeaORM` connections.
    ///
    /// Each component gets its own `DatabaseConnection` handle: under the `SeaORM`
    /// `mock` dev-feature `DatabaseConnection` drops `Clone`, so the pattern from
    /// `AppState::from_connections` carries over. The `UoW` provider takes a
    /// dedicated connection (required to open write transactions for
    /// `ensure_current_month`).
    ///
    /// The `income` expectation is the UNWIRED placeholder
    /// (`UnwiredIncomeStub`) until real income wiring (B4) lands. This seam is
    /// **UNSAFE once an actual income row exists**: the `D5` rollover formula
    /// `net = (actual_income - expected_income) + expense_remaining` would, with a
    /// zero expectation, roll a month forward inflated by the full income amount
    /// (`BUDGET-ROLLOVER-INTEGRITY-1`). It is correct *today* only because no
    /// production path writes an income row yet; `UnwiredIncomeStub` reports
    /// itself untrustworthy so `MonthLifecycleService::prior_month_net` FAILS LOUD
    /// (`DomainError::UntrustworthyIncomeRollover`, `SPIRIT-ROBUSTNESS-1`) rather
    /// than commit a wrong rollover if that assumption is ever violated. The
    /// prerequisite before income rows are trustworthy is wiring real
    /// `ConfigDrivenIncomeExpectation` (B4) from the persisted `paycheck_config`.
    pub fn new(
        months: Arc<dyn MonthRepository>,
        budgets: Arc<dyn BudgetRepository>,
        transactions: Arc<dyn TransactionRepository>,
        funds: Arc<dyn budget_domain::repositories::FundRepository>,
        lifecycle: Arc<budget_app_services::MonthLifecycleService>,
    ) -> Self {
        Self {
            months,
            budgets,
            transactions,
            funds,
            lifecycle,
        }
    }

    /// Wire from five independent `DatabaseConnection` handles.
    ///
    /// Called from `budget-server/main.rs` after `run_pending_migrations`.
    #[must_use]
    pub fn from_connections(
        months_db: DatabaseConnection,
        budgets_db: DatabaseConnection,
        transactions_db: DatabaseConnection,
        funds_db: DatabaseConnection,
        uow_db: DatabaseConnection,
    ) -> Self {
        use budget_app_services::{IncomeExpectation, MonthLifecycleService, UnwiredIncomeStub};
        use budget_domain::repositories::FundRepository;
        use budget_domain::uow::UowProvider;
        use budget_infrastructure::{PostgresFundRepository, SeaOrmUowProvider};

        let months: Arc<dyn MonthRepository> = Arc::new(PostgresMonthRepository::new(months_db));
        let budgets: Arc<dyn BudgetRepository> =
            Arc::new(PostgresBudgetRepository::new(budgets_db));
        let transactions: Arc<dyn TransactionRepository> =
            Arc::new(PostgresTransactionRepository::new(transactions_db));
        let funds: Arc<dyn FundRepository> = Arc::new(PostgresFundRepository::new(funds_db));

        // Unwired income seam for B4/ledger (read-only view, see doc above). This
        // placeholder reports itself untrustworthy so a committed rollover can
        // never silently inflate by an un-subtracted income amount.
        let income: Arc<dyn IncomeExpectation> = Arc::new(UnwiredIncomeStub::new());

        let uow: Arc<dyn UowProvider> = Arc::new(SeaOrmUowProvider::new(uow_db));

        let lifecycle = Arc::new(MonthLifecycleService::new(
            months.clone(),
            budgets.clone(),
            transactions.clone(),
            funds.clone(),
            uow,
            income,
        ));

        Self {
            months,
            budgets,
            transactions,
            funds,
            lifecycle,
        }
    }

    /// Extract the `MonthViewState` from the current server-function request.
    ///
    /// # Errors
    ///
    /// Returns a `500` error if the extension is absent (wiring fault in `main.rs`).
    pub async fn extract() -> Result<Self, dioxus::prelude::ServerFnError> {
        use dioxus::fullstack::FullstackContext;
        use dioxus::fullstack::axum::Extension;

        let Extension(state) = FullstackContext::extract::<Extension<Self>, _>()
            .await
            .map_err(|_| dioxus::prelude::ServerFnError::ServerError {
                message: "month-view state unavailable (wiring fault)".to_owned(),
                code: 500,
                details: None,
            })?;
        Ok(state)
    }
}

// ---------------------------------------------------------------------------
// TriageState — the server state for the BACKEND-3 Pull / Pending / triage fns
// ---------------------------------------------------------------------------

/// Server state for the Pull -> Pending -> triage server functions
/// (`SPEC §7`, BACKEND-3): the manual Plaid pull, the triage inbox read, and the
/// atomic triage write.
///
/// Holds the [`PlaidSyncService`] (the manual Pull, `SPEC §6`) and the
/// [`TriageService`] (the inbox + atomic triage, `SPEC §7`). The
/// [`PlaidSyncService`] is OPTIONAL: it requires the Plaid credentials + Key Vault
/// URL, which are absent in a local dev run without bank linking. When `None`, the
/// Pull server function returns a clear `503` (the inbox + triage still work over
/// whatever rows already exist). `Arc`-backed so cloning into the Axum `Extension`
/// is cheap.
#[derive(Clone)]
pub struct TriageState {
    /// The triage use case: the inbox read + the atomic triage write (`SPEC §7`).
    pub triage: Arc<budget_app_services::TriageService>,
    /// The Plaid sync use case driving the manual Pull (`SPEC §6`). `None` when
    /// Plaid is not configured (no credentials / vault) — the Pull fn then 503s.
    pub plaid: Option<Arc<budget_app_services::PlaidSyncService>>,
}

impl TriageState {
    /// Assemble from collaborators (used directly by tests injecting fakes).
    #[must_use]
    pub fn new(
        triage: Arc<budget_app_services::TriageService>,
        plaid: Option<Arc<budget_app_services::PlaidSyncService>>,
    ) -> Self {
        Self { triage, plaid }
    }

    /// Wire the production state from independent live `SeaORM` connections.
    ///
    /// The triage service is always wired. The Plaid sync service is wired only when
    /// the Plaid credentials (`PLAID_CLIENT_ID` / `PLAID_SECRET`) AND the Key Vault
    /// URL (`KEY_VAULT_URL`) are present in the environment; otherwise it is `None`
    /// and the Pull server function returns `503` (bank linking is a deploy-time
    /// concern, `SPEC §6`/`§12`). Each component takes its own connection (the
    /// `SeaORM` `mock`-feature `Clone` caveat, as elsewhere in this module).
    ///
    /// # Errors
    /// Returns the Key Vault construction error string if `KEY_VAULT_URL` is set but
    /// invalid.
    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    pub fn from_connections(
        transactions_db: DatabaseConnection,
        funds_db: DatabaseConnection,
        fund_budgets_db: DatabaseConnection,
        fund_uow_db: DatabaseConnection,
        triage_uow_db: DatabaseConnection,
        plaid_items_db: DatabaseConnection,
        plaid_months_db: DatabaseConnection,
        plaid_users_db: DatabaseConnection,
        plaid_txns_db: DatabaseConnection,
        plaid_engine_uow_db: DatabaseConnection,
        plaid_sync_uow_db: DatabaseConnection,
    ) -> Result<Self, String> {
        use budget_app_services::{FundService, TriageService};
        use budget_domain::repositories::{
            BudgetRepository, FundRepository, TransactionRepository,
        };
        use budget_domain::uow::UowProvider;
        use budget_infrastructure::{
            PostgresBudgetRepository, PostgresFundRepository, PostgresTransactionRepository,
            SeaOrmUowProvider,
        };

        // FundService (the money-math home the triage treatments reuse).
        let fund_txns: Arc<dyn TransactionRepository> =
            Arc::new(PostgresTransactionRepository::new(transactions_db));
        let funds: Arc<dyn FundRepository> = Arc::new(PostgresFundRepository::new(funds_db));
        let fund_budgets: Arc<dyn BudgetRepository> =
            Arc::new(PostgresBudgetRepository::new(fund_budgets_db));
        let fund_uow: Arc<dyn UowProvider> = Arc::new(SeaOrmUowProvider::new(fund_uow_db));
        let fund_service = Arc::new(FundService::new(
            Arc::clone(&funds),
            Arc::clone(&fund_txns),
            fund_budgets,
            fund_uow,
        ));

        // The triage service's own transaction repo + unit of work (its atomic
        // category/comment + treatment write).
        let triage_txns: Arc<dyn TransactionRepository> =
            Arc::new(PostgresTransactionRepository::new(plaid_txns_db));
        let triage_uow: Arc<dyn UowProvider> = Arc::new(SeaOrmUowProvider::new(triage_uow_db));
        let triage = Arc::new(TriageService::new(
            Arc::clone(&triage_txns),
            Arc::clone(&fund_service),
            triage_uow,
        ));

        // Plaid is wired only when configured.
        let plaid = Self::wire_plaid(
            plaid_items_db,
            plaid_months_db,
            plaid_users_db,
            plaid_engine_uow_db,
            plaid_sync_uow_db,
            triage_txns,
        )?;

        Ok(Self::new(triage, plaid))
    }

    /// Wire the [`PlaidSyncService`], selecting the Plaid client + secret vault
    /// by an explicit opt-in (`STAGE-1` local testing):
    ///
    /// - **`PLAID_MODE=mock`** (explicit opt-in): wire the in-process
    ///   [`MockPlaidApi`] + [`InMemorySecretVault`] so the whole Pull -> Pending
    ///   -> triage path runs LOCALLY with fake bank data and NO real Plaid / Neon
    ///   / Azure. A clear `WARN` is logged at startup. Requires NO Plaid
    ///   credentials or Key Vault URL.
    /// - **anything else / unset** (the default/production path, unchanged): wire
    ///   the real [`HttpPlaidApi`] + [`AzureKeyVault`] iff the credentials
    ///   (`PLAID_CLIENT_ID` / `PLAID_SECRET`) AND the Key Vault URL
    ///   (`KEY_VAULT_URL`) are present; otherwise `Ok(None)` and the Pull server
    ///   function returns `503` (bank linking is a deploy-time concern).
    ///
    /// CRITICAL SAFETY (`STAGE-1`): the mock is OFF by default. Only the exact
    /// string `PLAID_MODE=mock` selects it; a misconfigured prod (the var unset,
    /// or any other value) keeps the real client + real Key Vault. The mock can
    /// never be reached silently.
    fn wire_plaid(
        plaid_items_db: DatabaseConnection,
        plaid_months_db: DatabaseConnection,
        plaid_users_db: DatabaseConnection,
        plaid_engine_uow_db: DatabaseConnection,
        plaid_sync_uow_db: DatabaseConnection,
        plaid_txns: Arc<dyn budget_domain::repositories::TransactionRepository>,
    ) -> Result<Option<Arc<budget_app_services::PlaidSyncService>>, String> {
        use budget_app_services::PlaidSyncService;
        use budget_domain::auth::SecretVault;
        use budget_domain::plaid_api::{PlaidApi, PlaidSyncEngine};
        use budget_domain::repositories::{MonthRepository, PlaidItemRepository, UserRepository};
        use budget_domain::uow::UowProvider;
        use budget_infrastructure::{
            AzureKeyVault, HttpPlaidApi, InMemorySecretVault, MockPlaidApi, PlaidCredentials,
            PlaidEnvironment, PostgresMonthRepository, PostgresPlaidItemRepository,
            PostgresUserRepository, SeaOrmPlaidSyncEngine, SeaOrmUowProvider,
        };

        // Select the Plaid client + secret vault. The mock is reached ONLY by the
        // exact `PLAID_MODE=mock` opt-in; every other case (incl. unset) takes the
        // real, unchanged production path.
        let mock_mode = std::env::var("PLAID_MODE").as_deref() == Ok("mock");
        let (plaid_api, vault): (Arc<dyn PlaidApi>, Arc<dyn SecretVault>) = if mock_mode {
            // One clear, loud line so an operator can never mistake a mock run for
            // a real one. No secret material (there is none) reaches the log.
            tracing::warn!(
                "PLAID_MODE=mock — using the LOCAL MockPlaidApi + in-memory secret store \
                 (fake bank data; NO real Plaid / Key Vault). This is a local-testing path; \
                 it must NEVER be set in production."
            );
            (
                Arc::new(MockPlaidApi::new()),
                Arc::new(InMemorySecretVault::new()),
            )
        } else {
            let (Ok(client_id), Ok(secret), Ok(vault_url)) = (
                std::env::var("PLAID_CLIENT_ID"),
                std::env::var("PLAID_SECRET"),
                std::env::var("KEY_VAULT_URL"),
            ) else {
                // Not configured: the Pull server function will 503. The inbox +
                // triage still operate over existing rows.
                return Ok(None);
            };

            let environment = if std::env::var("PLAID_ENV").as_deref() == Ok("production") {
                PlaidEnvironment::Production
            } else {
                PlaidEnvironment::Sandbox
            };
            let api: Arc<dyn PlaidApi> = Arc::new(HttpPlaidApi::with_default_client(
                PlaidCredentials { client_id, secret },
                environment,
            ));
            let vault: Arc<dyn SecretVault> =
                Arc::new(AzureKeyVault::new(&vault_url).map_err(|e| e.to_string())?);
            (api, vault)
        };

        // The repositories + unit-of-work wiring is identical for both paths: only
        // the Plaid client + secret vault differ above.
        let plaid_items: Arc<dyn PlaidItemRepository> =
            Arc::new(PostgresPlaidItemRepository::new(plaid_items_db));
        let plaid_months: Arc<dyn MonthRepository> =
            Arc::new(PostgresMonthRepository::new(plaid_months_db));
        let plaid_users: Arc<dyn UserRepository> =
            Arc::new(PostgresUserRepository::new(plaid_users_db));
        let engine_uow: Arc<dyn UowProvider> =
            Arc::new(SeaOrmUowProvider::new(plaid_engine_uow_db));
        let sync_uow: Arc<dyn UowProvider> = Arc::new(SeaOrmUowProvider::new(plaid_sync_uow_db));

        let engine: Arc<dyn PlaidSyncEngine> = Arc::new(SeaOrmPlaidSyncEngine::new(
            Arc::clone(&plaid_api),
            plaid_txns,
            Arc::clone(&plaid_months),
            Arc::clone(&plaid_items),
            engine_uow,
        ));

        Ok(Some(Arc::new(PlaidSyncService::new(
            plaid_api,
            engine,
            vault,
            plaid_items,
            plaid_users,
            sync_uow,
        ))))
    }

    /// Extract the `TriageState` from the current server-function request.
    ///
    /// # Errors
    ///
    /// Returns a `500` error if the extension is absent (wiring fault in `main.rs`).
    pub async fn extract() -> Result<Self, dioxus::prelude::ServerFnError> {
        use dioxus::fullstack::FullstackContext;
        use dioxus::fullstack::axum::Extension;

        let Extension(state) = FullstackContext::extract::<Extension<Self>, _>()
            .await
            .map_err(|_| dioxus::prelude::ServerFnError::ServerError {
                message: "triage state unavailable (wiring fault)".to_owned(),
                code: 500,
                details: None,
            })?;
        Ok(state)
    }
}

// ---------------------------------------------------------------------------
// PortfolioState — server state for the AI Portfolio Insights server functions
// ---------------------------------------------------------------------------

/// Server state for the AI Portfolio Insights server functions
/// (`docs/AI_FEATURE_DESIGN.md §Phase 2`).
///
/// Holds the manual position/cash-balance persistence adapters the positions UI
/// reads/writes through. The `market` provider (Phase 3) and the
/// `GeneratePortfolioReview` use-case (Phase 5) fields are added in their
/// respective phases — the struct is grown incrementally so each phase's
/// green-gate holds without forward-referencing types that do not exist yet. The
/// `from_connections` real/mock wiring lands in Phase 6 (`AI_MODE=mock`,
/// mirroring `PLAID_MODE=mock`); until then the state is assembled directly by
/// tests / the server edge.
///
/// `Arc`-backed so cloning into the Axum `Extension` is cheap.
#[derive(Clone)]
pub struct PortfolioState {
    /// The manual positions read/write adapter (`PositionRepository`).
    pub position_source: Arc<dyn budget_domain::repositories::PositionRepository>,
    /// The manual cash-balances read/write adapter (`CashBalanceRepository`),
    /// bound to the single app user (`SPEC §9`; see `ManualCashBalanceSource`).
    pub balance_source: Arc<dyn budget_domain::repositories::CashBalanceRepository>,
    /// The market-data provider for per-ticker quote resolution (`§Phase 3`).
    /// `MockMarketDataProvider` under (Phase 6) `AI_MODE=mock`; the real adapter
    /// is an Open Item.
    pub market: Arc<dyn budget_domain::portfolio::MarketDataProvider>,
    // The `GeneratePortfolioReview` use-case (`review_service`) is added in
    // Phase 5; `run_review` stays a stub until Phase 6 wires its body.
}

impl PortfolioState {
    /// Assemble from collaborators (used directly by tests + the server edge
    /// until the Phase-6 `from_connections` wiring lands).
    #[must_use]
    pub fn new(
        position_source: Arc<dyn budget_domain::repositories::PositionRepository>,
        balance_source: Arc<dyn budget_domain::repositories::CashBalanceRepository>,
        market: Arc<dyn budget_domain::portfolio::MarketDataProvider>,
    ) -> Self {
        Self {
            position_source,
            balance_source,
            market,
        }
    }

    /// Extract the `PortfolioState` from the current server-function request.
    ///
    /// # Errors
    /// Returns a `500` if the extension is absent (wiring fault in `main.rs`).
    pub async fn extract() -> Result<Self, dioxus::prelude::ServerFnError> {
        use dioxus::fullstack::FullstackContext;
        use dioxus::fullstack::axum::Extension;

        let Extension(state) = FullstackContext::extract::<Extension<Self>, _>()
            .await
            .map_err(|_| dioxus::prelude::ServerFnError::ServerError {
                message: "portfolio state unavailable (wiring fault)".to_owned(),
                code: 500,
                details: None,
            })?;
        Ok(state)
    }
}
