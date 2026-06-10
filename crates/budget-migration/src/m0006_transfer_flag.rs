//! m0006 — `transactions.is_transfer` + `transactions.plaid_category` (`SPEC §4.11`, D10).
//!
//! Adds two columns to `transactions`:
//!
//! - `is_transfer BOOLEAN NOT NULL DEFAULT false` — the internal-transfer exclusion
//!   flag (`BUDGET-TRANSFER-EXCLUDE-1`). An internal account movement (credit-card
//!   payment, checking↔savings transfer) is tracked but EXCLUDED from budget math on
//!   both legs. Set only at triage (the 4th Transfer treatment); never auto-applied.
//!   Default `false` for all existing rows (correct: no existing row is a transfer
//!   until the user marks it so).
//!
//! - `plaid_category TEXT NULL` — Plaid's `personal_finance_category.detailed`
//!   string captured at ingest (e.g. `LOAN_PAYMENTS_CREDIT_CARD_PAYMENT`,
//!   `TRANSFER_OUT`, `TRANSFER_IN`). Drives the triage AUTO-SUGGEST only — never
//!   used in budget math. Nullable; pre-existing rows and manually-entered rows carry
//!   `NULL` (no Plaid category available).
//!
//! ## Why a new migration (not an m0001 edit)
//!
//! m0001 is a shipped genesis migration; per `PROC-CI-MIGRATION-HYGIENE-1` schema
//! changes append as their own migration, never an in-place edit of an
//! already-run one (the journal keys off the name).
//!
//! ## Expand-only (non-breaking)
//!
//! - `is_transfer`: `NOT NULL DEFAULT false` — existing rows get `false`, the correct
//!   initial state. No `ALTER` of existing rows needed.
//! - `plaid_category`: nullable with no default — existing rows carry `NULL`, the
//!   correct initial state for rows that predate Plaid-category capture.
//!
//! Neither column invalidates existing rows or requires a migrate-reads phase
//! (`ARCH-EXPAND-CONTRACT-1`).
//!
//! ## SQL-DB-INDEX-2 — pending-inbox predicate index
//!
//! The pending-inbox query (`status='settled' AND category_id IS NULL AND
//! is_transfer = false`) filters on `is_transfer` in addition to the existing
//! `status` + `category_id` indexes. Because this is a flag column (low
//! cardinality), a standalone index on `is_transfer` alone has poor selectivity.
//! The existing partial index on `(status, category_id)` or its equivalent already
//! covers the high-selectivity filter; Postgres will apply the `is_transfer = false`
//! as a heap filter on the small result set. No additional index is needed for this
//! column in isolation. If a composite index spanning all three columns ever becomes
//! warranted (traffic analysis, slow-query data), add it as m000N at that time.
//!
//! ## Why raw SQL
//!
//! Consistent with m0001..m0005: `ADD COLUMN IF NOT EXISTS` idempotency reads
//! cleanest as raw DDL, with no `SeaORM` column-builder translation layer
//! (`PROC-CI-MIGRATION-HYGIENE-1`).

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(TRANSFER_UP_DDL)
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(TRANSFER_DOWN_DDL)
            .await?;
        Ok(())
    }
}

/// The expand DDL: add `is_transfer` and `plaid_category` to `transactions`.
///
/// Both `ADD COLUMN IF NOT EXISTS` guards make this idempotent on a re-run
/// (`PROC-CI-MIGRATION-HYGIENE-1`).
const TRANSFER_UP_DDL: &str = r"
-- SPEC §4.11 / D10 / BUDGET-TRANSFER-EXCLUDE-1:
-- is_transfer: internal account movement flag. NOT NULL DEFAULT false — all
-- existing rows are correctly initialized to 'not a transfer'. Set only at
-- triage; never auto-applied. Both legs of an internal transfer get this flag.
ALTER TABLE transactions
    ADD COLUMN IF NOT EXISTS is_transfer BOOLEAN NOT NULL DEFAULT false;

-- SPEC §4.11 / D10:
-- plaid_category: Plaid personal_finance_category.detailed string, captured at
-- ingest. Drives the Transfer triage AUTO-SUGGEST only (never budget math).
-- Nullable: existing rows and manual entries carry NULL (no Plaid category).
ALTER TABLE transactions
    ADD COLUMN IF NOT EXISTS plaid_category TEXT;
";

/// The contract DDL: drop `is_transfer` and `plaid_category`.
///
/// Both `DROP COLUMN IF EXISTS` guards make this idempotent (`PROC-CI-MIGRATION-HYGIENE-1`).
const TRANSFER_DOWN_DDL: &str = r"
ALTER TABLE transactions
    DROP COLUMN IF EXISTS is_transfer;

ALTER TABLE transactions
    DROP COLUMN IF EXISTS plaid_category;
";

#[cfg(test)]
mod tests {
    //! DB-free structural assertions over the m0006 DDL (`ORCH-NEW-PATH-TESTS-1`).
    //! The live apply/down round-trip is exercised by the `DATABASE_URL`-gated
    //! migration integration test.

    use super::{TRANSFER_DOWN_DDL, TRANSFER_UP_DDL};

    #[test]
    fn up_adds_is_transfer_not_null_default_false_idempotently() {
        assert!(
            TRANSFER_UP_DDL
                .contains("ADD COLUMN IF NOT EXISTS is_transfer BOOLEAN NOT NULL DEFAULT false"),
            "up must add is_transfer as BOOLEAN NOT NULL DEFAULT false with IF NOT EXISTS"
        );
    }

    #[test]
    fn up_adds_plaid_category_nullable_no_default_idempotently() {
        assert!(
            TRANSFER_UP_DDL.contains("ADD COLUMN IF NOT EXISTS plaid_category TEXT"),
            "up must add plaid_category as nullable TEXT with IF NOT EXISTS"
        );
        // Must be nullable — no NOT NULL constraint on plaid_category.
        let plaid_cat_line = TRANSFER_UP_DDL
            .lines()
            .find(|l| l.contains("ADD COLUMN IF NOT EXISTS plaid_category TEXT"))
            .unwrap_or("");
        assert!(
            !plaid_cat_line.contains("NOT NULL"),
            "plaid_category must be nullable (no NOT NULL)"
        );
        assert!(
            !plaid_cat_line.contains("DEFAULT"),
            "plaid_category must have no default"
        );
    }

    #[test]
    fn up_targets_transactions_table_for_both_columns() {
        let alter_count = TRANSFER_UP_DDL
            .lines()
            .filter(|l| l.contains("ALTER TABLE transactions"))
            .count();
        assert!(
            alter_count >= 2,
            "up must ALTER TABLE transactions at least twice (once per column)"
        );
    }

    #[test]
    fn down_drops_both_columns_idempotently() {
        assert!(
            TRANSFER_DOWN_DDL.contains("DROP COLUMN IF EXISTS is_transfer"),
            "down must drop is_transfer with IF EXISTS"
        );
        assert!(
            TRANSFER_DOWN_DDL.contains("DROP COLUMN IF EXISTS plaid_category"),
            "down must drop plaid_category with IF EXISTS"
        );
    }

    #[test]
    fn down_targets_transactions_table() {
        let alter_count = TRANSFER_DOWN_DDL
            .lines()
            .filter(|l| l.contains("ALTER TABLE transactions"))
            .count();
        assert!(
            alter_count >= 2,
            "down must ALTER TABLE transactions at least twice (once per drop)"
        );
    }
}
