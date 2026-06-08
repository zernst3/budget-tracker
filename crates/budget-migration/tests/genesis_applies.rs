//! Live-Postgres round-trip for the genesis migration.
//!
//! Gated on the `DATABASE_URL` env var: when unset (the default in CI until Zach
//! provisions Neon, SPEC §12), the test is a no-op so the suite stays green
//! without a database. When set, it proves the genesis migration is reversible
//! and idempotent (`PROC-CI-MIGRATION-HYGIENE-1`):
//!
//!   1. `up`   — applies the genesis schema from an empty database.
//!   2. `up`   again — no-op (the journal skips the applied step); idempotent.
//!   3. `down` — tears the whole schema back down.
//!   4. `up`   — re-applies cleanly from empty; reversible.
//!
//! Run against a throwaway database, e.g.:
//!
//! ```text
//! DATABASE_URL=postgres://localhost/budget_migration_test cargo test -p budget-migration
//! ```

// Test-only: panicking is the correct failure signal for a migration that does
// not apply against the live database. The workspace denies `panic` in
// production code; this integration target is exempt by intent.
#![allow(clippy::panic)]

use budget_migration::{Migrator, MigratorTrait};
use sea_orm::Database;

#[tokio::test]
async fn genesis_migration_applies_rolls_back_and_reapplies() {
    let Ok(url) = std::env::var("DATABASE_URL") else {
        eprintln!("DATABASE_URL unset — skipping live-Postgres genesis migration test");
        return;
    };

    let db = Database::connect(&url)
        .await
        .unwrap_or_else(|e| panic!("connect to {url}: {e}"));

    // Start from a known-empty state so the test is self-contained / re-runnable.
    Migrator::fresh(&db)
        .await
        .unwrap_or_else(|e| panic!("fresh (drop-all then up): {e}"));

    // Idempotency: re-running up applies nothing new and must not error.
    Migrator::up(&db, None)
        .await
        .unwrap_or_else(|e| panic!("second up (idempotent): {e}"));

    // Reversibility: down must tear the genesis schema fully back down.
    Migrator::down(&db, None)
        .await
        .unwrap_or_else(|e| panic!("down: {e}"));

    // Re-apply from empty proves down left a clean slate.
    Migrator::up(&db, None)
        .await
        .unwrap_or_else(|e| panic!("re-apply up after down: {e}"));
}
