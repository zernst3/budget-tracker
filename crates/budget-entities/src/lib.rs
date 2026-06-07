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
//!
//! Pure data shape — no business logic, no DTOs, no serde on `Model` (`ENTITIES-2`).
//! NUMERIC money columns map to `rust_decimal::Decimal` (`BUDGET-MONEY-1`), never to
//! `f32`/`f64`.

pub mod accounts;
pub mod budgets;
pub mod categories;
pub mod funds;
pub mod months;
pub mod paycheck_config;
pub mod plaid_items;
pub mod repayment_obligations;
pub mod transactions;
pub mod users;
