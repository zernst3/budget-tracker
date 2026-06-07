//! Translation layer between `SeaORM` Model types and domain types.
//!
//! Sits in its own crate (`MAPPER-1`) so neither `budget-domain` nor
//! `budget-infrastructure` depends on the other. This is the single boundary
//! where the Plaid sign convention is normalized (`BUDGET-PLAID-SIGN-1`: Plaid
//! positive-outflow flips to internal negative-expense exactly once, here).
//!
//! Mappers land in build step 2 (see `.build-progress.md`).
