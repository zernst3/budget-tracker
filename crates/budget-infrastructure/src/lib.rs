//! Concrete adapters for the budget tracker.
//!
//! `SeaORM` repository implementations + the `SeaOrmUow` / `SeaOrmUowProvider`
//! unit-of-work primitive (`REPO-*`), the Azure Key Vault client (Plaid token
//! stored only as a secret reference, `BUDGET-PLAID-TOKEN-VAULT-1`), and the
//! Plaid Transactions-only client. Depends on domain, entities, and mappers.
//!
//! Adapters land in build step 3+ (see `.build-progress.md`).
