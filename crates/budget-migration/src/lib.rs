//! `SeaORM` schema migrations for the budget tracker.
//!
//! Schema source of truth: `SPEC.md` §5 (table/column shape) + §12 (resolved
//! DB-level constraints). The `SeaORM` Model types in `budget-entities` mirror
//! this schema but, per `ENTITIES-7`/`ENTITIES-8`, every constraint the entity
//! macro cannot express (composite uniques, partial/conditional uniques, real
//! Postgres enum types, FK + lookup indexes) is materialized **here** and only
//! documented in prose on the entity. This crate is the single place those
//! invariants actually live in the database.
//!
//! ## Runner
//!
//! [`Migrator`] implements `MigratorTrait`; it is the ordered list of every
//! migration. The server runs it at startup via `Migrator::up(&db, None)` (see
//! the `budget-infrastructure` re-export and `run_pending`). `sea-orm-migration`
//! tracks applied migrations in its own `seaql_migrations` journal table, so
//! re-running is a no-op for already-applied steps — the runner itself is
//! idempotent (`PROC-CI-MIGRATION-HYGIENE-1`).
//!
//! ## Conventions encoded
//!
//! - `BUDGET-MONEY-1` / `DOMAIN-8`: every monetary column is Postgres `NUMERIC`,
//!   never a float. `decimal_len(_, 18, 2)` gives 16 integer + 2 fraction digits.
//! - `ENTITIES-12`: each pg-enum column is backed by a real Postgres `ENUM`
//!   type created with `Type::create()` (m0001), not a `VARCHAR`/`TEXT` + check.
//! - `SQL-DB-INDEX-1`: every foreign-key column gets an index in the **same**
//!   migration that creates the column, declared adjacent to the table.
//! - `SQL-DB-INDEX-2`: every column a repository read path filters / orders on
//!   gets a supporting index (the lookup indexes in m0001).
//! - The four `SPEC` §12 DB constraints (`BUDGET-ROLLOVER-INTEGRITY-1`,
//!   `BUDGET-IDEMPOTENT-MONTH-INIT-1`, Plaid dedup): partial unique on
//!   `categories(budget_id) WHERE is_rollover_bucket`, partial unique on
//!   `transactions(month_id) WHERE is_rollover`, unique
//!   `transactions.plaid_transaction_id`, and `UNIQUE(user_id, year, month)` on
//!   `months`.
//! - `ARCH-EXPAND-CONTRACT-1`: any future breaking change (drop/rename/NOT NULL)
//!   is split across migrations (expand → migrate reads → contract). Migrations
//!   are schema-only; never seed user data here. m0001 is the genesis create, so
//!   it has no prior shape to expand from.

pub use sea_orm_migration::prelude::*;

mod m0001_genesis_schema;
mod m0002_auth_schema;
mod m0003_transaction_match_link;
mod m0004_deficit_financing;
mod m0005_transaction_comment;

/// The ordered migration list run by the server at startup.
///
/// Append new migrations to the end of [`MigratorTrait::migrations`]; never edit
/// or reorder an already-shipped migration (the journal keys off the name).
pub struct Migrator;

#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![
            Box::new(m0001_genesis_schema::Migration),
            Box::new(m0002_auth_schema::Migration),
            Box::new(m0003_transaction_match_link::Migration),
            Box::new(m0004_deficit_financing::Migration),
            Box::new(m0005_transaction_comment::Migration),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::{Migrator, MigratorTrait};

    /// Every migration name in the Migrator is unique — the journal keys off the
    /// name, so a duplicate would silently skip a real migration
    /// (`PROC-CI-MIGRATION-HYGIENE-1`).
    #[test]
    fn migration_names_are_unique_and_nonempty() {
        let migrations = Migrator::migrations();
        assert!(!migrations.is_empty(), "Migrator has no migrations");

        let mut names: Vec<&str> = migrations.iter().map(|m| m.name()).collect();
        let count = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), count, "duplicate migration name in Migrator");
    }
}
