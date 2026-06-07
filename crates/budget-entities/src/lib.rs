//! `SeaORM` persistence Model types for the budget tracker.
//!
//! One module per Postgres table (`ENTITIES-13`), mirroring the schema in
//! `SPEC.md` §5 (`users`, `budgets`, `categories`, `accounts`, `plaid_items`,
//! `months`, `transactions`, `funds`, `repayment_obligations`,
//! `paycheck_config`). Pure data shape — no business logic, no DTOs. NUMERIC
//! money columns map to `rust_decimal::Decimal` (`BUDGET-MONEY-1`), never to
//! `f32`/`f64`.
//!
//! Entities land in build step 2 (see `.build-progress.md`).
