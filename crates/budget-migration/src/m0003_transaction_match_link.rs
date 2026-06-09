//! m0003 — expected-expense ↔ real-transaction match-link (`SPEC §4.10` / `§12`,
//! `BUDGET-SETTLE-ON-MATCH-1`).
//!
//! Adds the nullable, self-referential `matched_transaction_id` column to
//! `transactions`. It is set on an `expected` placeholder row to record the real
//! transaction that settled it, so the placeholder/real pair counts exactly once
//! (`BUDGET-NO-DOUBLE-CHARGE-1`) and the match can be reversed if the real charge
//! is later removed (Plaid `removed` → restore the placeholder, clear the link).
//!
//! ## Why a new migration (not an m0001 edit)
//!
//! m0001 is a shipped genesis migration; per `ARCH-EXPAND-CONTRACT-1` /
//! `PROC-CI-MIGRATION-HYGIENE-1` a new column is appended as its own migration,
//! never an in-place edit of an already-run one (the journal keys off the name).
//! This is a pure expand step: a nullable column with no backfill, so it is
//! non-breaking and needs no migrate/contract follow-up.
//!
//! ## FK shape
//!
//! `matched_transaction_id` is a self-referential FK → `transactions(id)` with
//! `ON DELETE SET NULL`: when the real (matched) transaction is hard-deleted out
//! of band, the link clears rather than cascading the placeholder away — the
//! placeholder must survive so it can be restored (`BUDGET-SETTLE-ON-MATCH-1`).
//! The application reverse-path (Plaid `removed`) clears the link explicitly and
//! restores the placeholder before the row is deleted; the `ON DELETE SET NULL`
//! is the DB-level backstop for any other deletion path.
//!
//! A partial index on the column (`WHERE matched_transaction_id IS NOT NULL`)
//! supports the reverse-path lookup "find the placeholder matched to this real
//! txn" (`SQL-DB-INDEX-2`) while staying small (only matched placeholders are
//! indexed). It is declared in the same migration as the column (`SQL-DB-INDEX-1`
//! spirit: index the FK in the migration that adds it).
//!
//! ## Why raw SQL
//!
//! Same rationale as m0001 / m0002: a self-referential FK with a named constraint,
//! a partial index, and explicit `IF NOT EXISTS` idempotency are clearest as raw
//! DDL (`PROC-CI-MIGRATION-HYGIENE-1`).

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(MATCH_LINK_UP_DDL)
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(MATCH_LINK_DOWN_DDL)
            .await?;
        Ok(())
    }
}

/// The expand DDL: add the nullable self-referential FK column + its partial
/// lookup index. `ADD COLUMN IF NOT EXISTS` + `CREATE INDEX IF NOT EXISTS` keep
/// the step idempotent (`PROC-CI-MIGRATION-HYGIENE-1`).
const MATCH_LINK_UP_DDL: &str = r"
-- ============== transactions.matched_transaction_id (SPEC §4.10) =======
-- D9/§12, BUDGET-SETTLE-ON-MATCH-1: the real txn that settled this 'expected'
-- placeholder. Set on the placeholder row; NULL for every other row. Nullable,
-- self-referential FK with ON DELETE SET NULL so deleting the matched real txn
-- clears the link (and the placeholder is restored) rather than cascading the
-- placeholder away.
ALTER TABLE transactions
    ADD COLUMN IF NOT EXISTS matched_transaction_id UUID;

-- Self-referential FK. Named so `down` can drop it deterministically. Guarded by
-- a duplicate-object catch so a re-run is a no-op (ADD CONSTRAINT has no
-- IF NOT EXISTS in stable Postgres).
DO $$
BEGIN
    ALTER TABLE transactions
        ADD CONSTRAINT fk_transactions_matched_transaction_id
        FOREIGN KEY (matched_transaction_id)
        REFERENCES transactions (id)
        ON DELETE SET NULL;
EXCEPTION
    WHEN duplicate_object THEN NULL;
END
$$;

-- SQL-DB-INDEX-2: the reverse path looks a placeholder up BY the real txn it is
-- matched to. Partial (only matched rows) keeps it small.
CREATE INDEX IF NOT EXISTS ix_transactions_matched_transaction_id
    ON transactions (matched_transaction_id)
    WHERE matched_transaction_id IS NOT NULL;
";

/// The contract DDL: drop the index, the FK constraint, then the column, in
/// reverse dependency order, each `IF EXISTS` (`PROC-CI-MIGRATION-HYGIENE-1`).
const MATCH_LINK_DOWN_DDL: &str = r"
DROP INDEX IF EXISTS ix_transactions_matched_transaction_id;
ALTER TABLE transactions
    DROP CONSTRAINT IF EXISTS fk_transactions_matched_transaction_id;
ALTER TABLE transactions
    DROP COLUMN IF EXISTS matched_transaction_id;
";

#[cfg(test)]
mod tests {
    //! DB-free structural assertions over the m0003 DDL (`ORCH-NEW-PATH-TESTS-1`).
    //! The live apply/down round-trip is exercised by the `DATABASE_URL`-gated
    //! migration integration test.

    use super::{MATCH_LINK_DOWN_DDL, MATCH_LINK_UP_DDL};

    #[test]
    fn up_adds_nullable_column() {
        assert!(
            MATCH_LINK_UP_DDL.contains("ADD COLUMN IF NOT EXISTS matched_transaction_id UUID"),
            "missing nullable matched_transaction_id column add",
        );
        // No NOT NULL — the column must be nullable (only matched placeholders set it).
        assert!(
            !MATCH_LINK_UP_DDL.contains("matched_transaction_id UUID NOT NULL"),
            "matched_transaction_id must be nullable",
        );
    }

    #[test]
    fn up_self_referential_fk_set_null() {
        assert!(MATCH_LINK_UP_DDL.contains("fk_transactions_matched_transaction_id"));
        assert!(
            MATCH_LINK_UP_DDL.contains("REFERENCES transactions (id)"),
            "FK must be self-referential to transactions(id)",
        );
        assert!(
            MATCH_LINK_UP_DDL.contains("ON DELETE SET NULL"),
            "deleting the matched txn must clear the link, not cascade",
        );
    }

    #[test]
    fn up_partial_lookup_index_present() {
        assert!(MATCH_LINK_UP_DDL.contains("ix_transactions_matched_transaction_id"));
        assert!(
            MATCH_LINK_UP_DDL.contains("WHERE matched_transaction_id IS NOT NULL"),
            "the reverse-path index must be partial (matched rows only)",
        );
    }

    #[test]
    fn up_is_idempotent() {
        // ADD COLUMN / CREATE INDEX guarded; ADD CONSTRAINT inside a duplicate_object catch.
        assert!(MATCH_LINK_UP_DDL.contains("ADD COLUMN IF NOT EXISTS"));
        assert!(MATCH_LINK_UP_DDL.contains("CREATE INDEX IF NOT EXISTS"));
        assert!(MATCH_LINK_UP_DDL.contains("WHEN duplicate_object THEN NULL"));
    }

    #[test]
    fn down_drops_in_reverse_order_if_exists() {
        let drop_index =
            MATCH_LINK_DOWN_DDL.find("DROP INDEX IF EXISTS ix_transactions_matched_transaction_id");
        let drop_fk = MATCH_LINK_DOWN_DDL
            .find("DROP CONSTRAINT IF EXISTS fk_transactions_matched_transaction_id");
        let drop_col = MATCH_LINK_DOWN_DDL.find("DROP COLUMN IF EXISTS matched_transaction_id");
        assert!(drop_index.is_some(), "down must drop the index");
        assert!(drop_fk.is_some(), "down must drop the FK constraint");
        assert!(drop_col.is_some(), "down must drop the column");
        assert!(
            drop_index < drop_fk && drop_fk < drop_col,
            "down must drop index -> constraint -> column in reverse dependency order",
        );
    }
}
