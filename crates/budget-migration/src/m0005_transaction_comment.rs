//! m0005 — `transactions.comment` column (`SPEC §5`, `§7`).
//!
//! Adds a nullable `TEXT` column `comment` to `transactions`. This is the
//! user's free-text note on an expense, distinct from `description` (the
//! Plaid/merchant string). It is inline-editable in the ledger and settable
//! during Pending triage (`SPEC §7`).
//!
//! ## Why a new migration (not an m0001 edit)
//!
//! m0001 is a shipped genesis migration; per `ARCH-EXPAND-CONTRACT-1` /
//! `PROC-CI-MIGRATION-HYGIENE-1` schema changes append as their own migration,
//! never an in-place edit of an already-run one (the journal keys off the name).
//!
//! ## Expand-only (non-breaking)
//!
//! Adding a nullable column with no default never invalidates existing rows.
//! No migrate-reads / contract follow-up is required
//! (`ARCH-EXPAND-CONTRACT-1`).
//!
//! ## Why raw SQL
//!
//! Consistent with m0001..m0004: `ADD COLUMN IF NOT EXISTS` idempotency reads
//! cleanest as a raw DDL statement, with no `SeaORM` column-builder translation
//! layer in the way (`PROC-CI-MIGRATION-HYGIENE-1`).

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(COMMENT_UP_DDL)
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(COMMENT_DOWN_DDL)
            .await?;
        Ok(())
    }
}

/// The expand DDL: add the nullable `comment` column.
///
/// `ADD COLUMN IF NOT EXISTS` is idempotent — a re-run on an already-migrated
/// DB is a no-op (`PROC-CI-MIGRATION-HYGIENE-1`).
const COMMENT_UP_DDL: &str = r"
-- SPEC §5 / §7: user's free-text note on an expense, distinct from
-- `description` (the Plaid/merchant string). Inline-editable in the ledger;
-- also settable during Pending triage. Nullable with no default — existing
-- rows carry NULL (no note yet), which is the correct initial state.
ALTER TABLE transactions
    ADD COLUMN IF NOT EXISTS comment TEXT;
";

/// The contract DDL: drop the `comment` column.
///
/// `DROP COLUMN IF EXISTS` is idempotent (`PROC-CI-MIGRATION-HYGIENE-1`).
const COMMENT_DOWN_DDL: &str = r"
ALTER TABLE transactions
    DROP COLUMN IF EXISTS comment;
";

#[cfg(test)]
mod tests {
    //! DB-free structural assertions over the m0005 DDL (`ORCH-NEW-PATH-TESTS-1`).
    //! The live apply/down round-trip is exercised by the `DATABASE_URL`-gated
    //! migration integration test.

    use super::{COMMENT_DOWN_DDL, COMMENT_UP_DDL};

    #[test]
    fn up_adds_nullable_comment_column_idempotently() {
        assert!(
            COMMENT_UP_DDL.contains("ADD COLUMN IF NOT EXISTS comment TEXT"),
            "up must add comment as a nullable TEXT column with IF NOT EXISTS guard"
        );
    }

    #[test]
    fn up_targets_transactions_table() {
        assert!(
            COMMENT_UP_DDL.contains("ALTER TABLE transactions"),
            "up must target the transactions table"
        );
    }

    #[test]
    fn up_comment_has_no_default_and_is_nullable() {
        // The column line must NOT include NOT NULL or DEFAULT (it is nullable
        // with no default — existing rows carry NULL, the correct initial state).
        let col_line = COMMENT_UP_DDL
            .lines()
            .find(|l| l.contains("ADD COLUMN IF NOT EXISTS comment TEXT"))
            .unwrap_or("");
        assert!(
            !col_line.contains("NOT NULL"),
            "comment must be nullable (no NOT NULL)"
        );
        assert!(
            !col_line.contains("DEFAULT"),
            "comment must have no default"
        );
    }

    #[test]
    fn down_drops_comment_column_idempotently() {
        assert!(
            COMMENT_DOWN_DDL.contains("DROP COLUMN IF EXISTS comment"),
            "down must drop comment with IF EXISTS guard"
        );
    }

    #[test]
    fn down_targets_transactions_table() {
        assert!(
            COMMENT_DOWN_DDL.contains("ALTER TABLE transactions"),
            "down must target the transactions table"
        );
    }
}
