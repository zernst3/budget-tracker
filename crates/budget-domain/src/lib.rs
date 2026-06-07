//! Pure business logic for the budget tracker.
//!
//! Hexagonal core (`DOMAIN-1`): no framework, no async runtime, no ORM, no DB.
//! Holds the domain newtypes (incl. the `Money` type, `BUDGET-MONEY-1`), error
//! enums (thiserror), repository traits, and the budget invariants
//! (`BUDGET-NO-DOUBLE-CHARGE-1`, `BUDGET-STATUS-DRIVES-INCLUSION-1`, etc.).
//!
//! Schema + domain types land in build step 2 (see `.build-progress.md`).
