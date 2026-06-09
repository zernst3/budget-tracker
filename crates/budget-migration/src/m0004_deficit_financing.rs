//! m0004 — deficit financing (`SPEC §12` D9, `§5`, `BUDGET-DEFICIT-FINANCING-1`).
//!
//! Extends `repayment_obligations` so the same machinery can amortize an
//! accumulated monthly deficit, not just a single buffer-financed purchase
//! (`SPEC §4.9` D7). Three schema changes:
//!
//!   1. a new `obligation_source` pg-enum (`'large_purchase' | 'deficit'`,
//!      `ENTITIES-12`) + a `source` column, defaulted/backfilled to
//!      `'large_purchase'` (every existing row is a buffer-financed purchase);
//!   2. a nullable `origin_month_id` FK → `months(id)` — the closed month whose
//!      deficit was financed (`NULL` for a `large_purchase`); and
//!   3. `transaction_id` made NULLABLE — a deficit has no single source
//!      transaction (`NULL` for `'deficit'`; still set for `'large_purchase'`).
//!
//! ## Why a new migration (not an m0001 edit)
//!
//! m0001 is a shipped genesis migration; per `ARCH-EXPAND-CONTRACT-1` /
//! `PROC-CI-MIGRATION-HYGIENE-1` schema changes append as their own migration,
//! never an in-place edit of an already-run one (the journal keys off the name).
//!
//! ## Expand-only (non-breaking)
//!
//! All three changes are widenings: a new enum type, two nullable/defaulted
//! columns, and a `DROP NOT NULL` (relaxing a constraint never invalidates
//! existing rows). The `source` backfill runs in the same step via the column
//! `DEFAULT 'large_purchase'` plus an explicit `UPDATE` for any pre-existing
//! row, then `SET NOT NULL` so the column is total going forward. No
//! migrate-reads/contract follow-up is needed (`ARCH-EXPAND-CONTRACT-1`).
//!
//! ## Why raw SQL
//!
//! Same rationale as m0001..m0003: a guarded `CREATE TYPE` (Postgres has no
//! `CREATE TYPE IF NOT EXISTS`), a named FK constraint, FK index, and explicit
//! `IF NOT EXISTS` idempotency read clearest as raw DDL
//! (`PROC-CI-MIGRATION-HYGIENE-1`).

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(DEFICIT_FINANCING_UP_DDL)
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(DEFICIT_FINANCING_DOWN_DDL)
            .await?;
        Ok(())
    }
}

/// The expand DDL: new `obligation_source` enum, the `source` + `origin_month_id`
/// columns, the `transaction_id` `DROP NOT NULL`, the backfill, and the
/// `origin_month_id` FK index. Every step is idempotent (`CREATE TYPE` guarded by
/// a `duplicate_object` catch; `ADD COLUMN IF NOT EXISTS`; `ADD CONSTRAINT` in a
/// `duplicate_object` catch; `CREATE INDEX IF NOT EXISTS`; the `SET`/`DROP`
/// column-state changes are inherently re-runnable, `PROC-CI-MIGRATION-HYGIENE-1`).
const DEFICIT_FINANCING_UP_DDL: &str = r"
-- ============== obligation_source pg-enum (ENTITIES-12) ================
-- D9/§5: 'large_purchase' (the existing buffer-financed-purchase path) and
-- 'deficit' (the new accumulated-deficit path). Guarded: CREATE TYPE has no
-- IF NOT EXISTS, so a re-run swallows the duplicate (PROC-CI-MIGRATION-HYGIENE-1).
DO $$
BEGIN
    CREATE TYPE obligation_source AS ENUM ('large_purchase', 'deficit');
EXCEPTION
    WHEN duplicate_object THEN NULL;
END
$$;

-- ============== repayment_obligations.source (D9, §5) ==================
-- DEFAULT 'large_purchase' backfills every existing row (all current obligations
-- are buffer-financed purchases). The explicit UPDATE covers any row that somehow
-- predates the default. SET NOT NULL makes the column total going forward.
ALTER TABLE repayment_obligations
    ADD COLUMN IF NOT EXISTS source obligation_source NOT NULL DEFAULT 'large_purchase';

UPDATE repayment_obligations SET source = 'large_purchase' WHERE source IS NULL;

-- ============== repayment_obligations.origin_month_id (D9, §5) =========
-- The closed month whose accumulated deficit was financed; NULL for a
-- large_purchase. Nullable, self-evidently no backfill.
ALTER TABLE repayment_obligations
    ADD COLUMN IF NOT EXISTS origin_month_id UUID;

-- Named FK so `down` can drop it deterministically; guarded by a duplicate_object
-- catch (ADD CONSTRAINT has no IF NOT EXISTS in stable Postgres). ON DELETE
-- RESTRICT mirrors the existing fund/transaction FKs: an obligation pins its
-- origin month.
DO $$
BEGIN
    ALTER TABLE repayment_obligations
        ADD CONSTRAINT fk_repayment_obligations_origin_month_id
        FOREIGN KEY (origin_month_id)
        REFERENCES months (id)
        ON DELETE RESTRICT;
EXCEPTION
    WHEN duplicate_object THEN NULL;
END
$$;

-- SQL-DB-INDEX-1: index the FK column in the migration that adds it.
CREATE INDEX IF NOT EXISTS ix_repayment_obligations_origin_month_id
    ON repayment_obligations (origin_month_id)
    WHERE origin_month_id IS NOT NULL;

-- ============== repayment_obligations.transaction_id NULLABLE (D9) =====
-- A deficit obligation has no single source transaction; relax the NOT NULL.
-- Widening a constraint never invalidates existing rows (all of which have a
-- transaction_id set). DROP NOT NULL is re-runnable.
ALTER TABLE repayment_obligations
    ALTER COLUMN transaction_id DROP NOT NULL;
";

/// The contract DDL: reverse dependency order. Re-tighten `transaction_id`
/// (safe iff no deficit rows exist; in a clean test round-trip none do), drop the
/// `origin_month_id` index → FK → column, drop the `source` column, then drop the
/// enum type. Each `IF EXISTS` (`PROC-CI-MIGRATION-HYGIENE-1`).
const DEFICIT_FINANCING_DOWN_DDL: &str = r"
ALTER TABLE repayment_obligations
    ALTER COLUMN transaction_id SET NOT NULL;

DROP INDEX IF EXISTS ix_repayment_obligations_origin_month_id;
ALTER TABLE repayment_obligations
    DROP CONSTRAINT IF EXISTS fk_repayment_obligations_origin_month_id;
ALTER TABLE repayment_obligations
    DROP COLUMN IF EXISTS origin_month_id;

ALTER TABLE repayment_obligations
    DROP COLUMN IF EXISTS source;

DROP TYPE IF EXISTS obligation_source;
";

#[cfg(test)]
mod tests {
    //! DB-free structural assertions over the m0004 DDL (`ORCH-NEW-PATH-TESTS-1`).
    //! The live apply/down round-trip is exercised by the `DATABASE_URL`-gated
    //! migration integration test.

    use super::{DEFICIT_FINANCING_DOWN_DDL, DEFICIT_FINANCING_UP_DDL};

    #[test]
    fn up_creates_obligation_source_enum_guarded() {
        assert!(
            DEFICIT_FINANCING_UP_DDL
                .contains("CREATE TYPE obligation_source AS ENUM ('large_purchase', 'deficit')"),
            "obligation_source enum must be created with both variants"
        );
        assert!(
            DEFICIT_FINANCING_UP_DDL.contains("WHEN duplicate_object THEN NULL"),
            "CREATE TYPE must be guarded (Postgres has no CREATE TYPE IF NOT EXISTS)"
        );
    }

    #[test]
    fn up_adds_source_column_defaulted_to_large_purchase() {
        assert!(
            DEFICIT_FINANCING_UP_DDL.contains(
                "ADD COLUMN IF NOT EXISTS source obligation_source NOT NULL DEFAULT 'large_purchase'"
            ),
            "source column must default/backfill existing rows to large_purchase"
        );
    }

    #[test]
    fn up_backfills_existing_rows_to_large_purchase() {
        assert!(
            DEFICIT_FINANCING_UP_DDL
                .contains("UPDATE repayment_obligations SET source = 'large_purchase'"),
            "existing rows must be backfilled to source='large_purchase'"
        );
    }

    #[test]
    fn up_adds_nullable_origin_month_fk_and_index() {
        assert!(
            DEFICIT_FINANCING_UP_DDL.contains("ADD COLUMN IF NOT EXISTS origin_month_id UUID"),
            "origin_month_id must be a nullable UUID column"
        );
        assert!(
            DEFICIT_FINANCING_UP_DDL.contains("REFERENCES months (id)"),
            "origin_month_id must FK months(id)"
        );
        assert!(
            DEFICIT_FINANCING_UP_DDL
                .contains("CREATE INDEX IF NOT EXISTS ix_repayment_obligations_origin_month_id"),
            "the origin_month_id FK column must be indexed in the same migration (SQL-DB-INDEX-1)"
        );
    }

    #[test]
    fn up_makes_transaction_id_nullable() {
        assert!(
            DEFICIT_FINANCING_UP_DDL.contains("ALTER COLUMN transaction_id DROP NOT NULL"),
            "transaction_id must be relaxed to NULLABLE for deficit obligations (D9)"
        );
    }

    #[test]
    fn down_reverses_in_dependency_order() {
        // Index dropped before its column; column dropped before the enum type.
        // Compare `Option<usize>` positions directly (Some(a) < Some(b)) to avoid
        // unwrap/expect (clippy::expect_used is denied in this crate).
        let idx = DEFICIT_FINANCING_DOWN_DDL
            .find("DROP INDEX IF EXISTS ix_repayment_obligations_origin_month_id");
        let col = DEFICIT_FINANCING_DOWN_DDL.find("DROP COLUMN IF EXISTS origin_month_id");
        let enum_drop = DEFICIT_FINANCING_DOWN_DDL.find("DROP TYPE IF EXISTS obligation_source");
        assert!(idx.is_some(), "down must drop the origin_month index");
        assert!(col.is_some(), "down must drop origin_month_id");
        assert!(enum_drop.is_some(), "down must drop the enum type");
        assert!(idx < col, "index must drop before its column");
        assert!(
            col < enum_drop,
            "source column path must drop before the enum type"
        );
        assert!(
            DEFICIT_FINANCING_DOWN_DDL.contains("ALTER COLUMN transaction_id SET NOT NULL"),
            "down re-tightens transaction_id"
        );
    }
}
