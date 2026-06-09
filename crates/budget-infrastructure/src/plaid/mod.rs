//! Plaid integration adapters (build step 8, `SPEC §6`).
//!
//! - [`HttpPlaidApi`] — the concrete reqwest-based [`budget_domain::plaid_api::PlaidApi`]
//!   client: create-link-token, exchange-public-token, and cursor
//!   `/transactions/sync`. Transactions(+Accounts) product only; the
//!   money-movement (Transfer) product is refused before any call
//!   (`BUDGET-PLAID-TOKEN-VAULT-1`, `SPEC §6`). The `access_token` is never
//!   logged.
//! - [`SeaOrmPlaidSyncEngine`] — the concrete [`budget_domain::plaid_api::PlaidSyncEngine`]:
//!   loops the cursor pages, maps each Plaid row through the mappers crate (the
//!   single sign-flip site, `BUDGET-PLAID-SIGN-1`), applies `added / modified /
//!   removed` idempotently (dedup by `plaid_transaction_id`), honors the genesis
//!   cutover guard (`BUDGET-CUTOVER-1`), reverses settlements on `removed`
//!   (`BUDGET-SETTLE-ON-MATCH-1`), persists the cursor, and runs the rolling
//!   30-day reconcile. All per-page writes commit atomically through the
//!   unit-of-work (`SERVICE-TX-1`).
//!
//! `PlaidSyncService` (in `budget-app-services`) orchestrates against the two
//! domain ports; tests substitute a mocked `PlaidApi` so NO live Plaid call runs
//! in a unit test (`SPEC §6`).

mod http_client;
mod sync_engine;

pub use http_client::{HttpPlaidApi, PlaidCredentials, PlaidEnvironment};
pub use sync_engine::SeaOrmPlaidSyncEngine;
