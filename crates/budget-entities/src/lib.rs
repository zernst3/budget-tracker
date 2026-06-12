//! `SeaORM` persistence Model types for the budget tracker.
//!
//! One module per Postgres table (`ENTITIES-13`), mirroring the schema in
//! `SPEC.md` §5. Tables:
//!   - [`users`] — sole user; `tracking_start_date` genesis boundary (`BUDGET-CUTOVER-1`)
//!   - [`budgets`] — versioned budget config; months reference by FK (SPEC §4.1)
//!   - [`categories`] — spending buckets; sinking funds + rollover bucket (SPEC §4.2, §4.7)
//!   - [`accounts`] — bank accounts, optionally linked via Plaid (SPEC §5)
//!   - [`plaid_items`] — one per linked institution; token stored as KV ref only (`BUDGET-PLAID-TOKEN-VAULT-1`)
//!   - [`months`] — open/closed lifecycle; `UNIQUE(user_id, year, month)` (SPEC §4.6)
//!   - [`transactions`] — central record type: expenses, income, rollovers, placeholders (SPEC §5)
//!   - [`funds`] — buffer/surplus virtual envelopes (SPEC §4.9)
//!   - [`repayment_obligations`] — buffer-draw repayment schedule (SPEC §4.9)
//!   - [`paycheck_config`] — income setup; one per user (SPEC §4.8)
//!   - [`webauthn_credentials`] — passkeys / biometric login; one user, many devices (SPEC §5, §9.1; `BUDGET-AUTH-GATE-1`)
//!   - [`positions`] — investment holdings; shares is a COUNT (AI Portfolio Insights, m0007); `drip_enabled` + `baseline_as_of` (DRIP, m0008)
//!   - [`cash_balances`] — labelled cash balances; reserved = buffer (`BUDGET-CASH-1`, m0007)
//!   - [`review_runs`] — append-only portfolio-review audit log (`SQL-AUDIT-COLUMNS-1`, m0007)
//!   - [`dividend_events`] — ticker-keyed dividend cache (DRIP, m0008)
//!   - [`drip_applications`] — append-only DRIP accretion chain (`SQL-AUDIT-COLUMNS-1`, m0008)
//!
//! Pure data shape — no business logic, no DTOs, no serde on `Model` (`ENTITIES-2`).
//! NUMERIC money columns map to `rust_decimal::Decimal` (`BUDGET-MONEY-1`), never to
//! `f32`/`f64`.

pub mod accounts;
pub mod budgets;
pub mod cash_balances;
pub mod categories;
pub mod dividend_events;
pub mod drip_applications;
pub mod funds;
pub mod months;
pub mod paycheck_config;
pub mod plaid_items;
pub mod positions;
pub mod repayment_obligations;
pub mod review_runs;
pub mod transactions;
pub mod users;
pub mod webauthn_credentials;
