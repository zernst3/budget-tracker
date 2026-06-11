//! m0007 — AI Portfolio Insights schema (`docs/AI_FEATURE_DESIGN.md §1.F`).
//!
//! Adds the three tables the portfolio-review feature persists against:
//!
//!   1. a `review_terminal_state` pg-enum (`ENTITIES-12`,
//!      `'completed' | 'no_verifiable_insights' | 'empty_portfolio' |
//!      'malformed_output'`) mirroring the entity's `ReviewTerminalStateEntity`;
//!   2. `positions` — investment holdings; `shares` a COUNT, `cost_basis`
//!      nullable money (`BUDGET-MONEY-1`); `account_type` reuses the existing
//!      `account_type` pg-enum (created in m0001). Composite unique
//!      `(user_id, ticker, account_label)` + FK index on `user_id`;
//!   3. `cash_balances` — labelled balances; `reserved` flag (`BUDGET-CASH-1`).
//!      Composite unique `(user_id, account_label)` + FK index;
//!   4. `review_runs` — the append-only review audit log
//!      (`SQL-AUDIT-COLUMNS-1`): no `created_by`/`modified_by`, no `updated_at`;
//!      JSONB `snapshot`/`outcomes`/`recommendations` (`§0.4` + addendum); FK
//!      index + a `(user_id, occurred_at DESC)` history index.
//!
//! ## Why a new migration (not an m0001 edit)
//!
//! m0001 is a shipped genesis migration; per `ARCH-EXPAND-CONTRACT-1` /
//! `PROC-CI-MIGRATION-HYGIENE-1` schema changes append as their own migration,
//! never an in-place edit of an already-run one (the journal keys off the name).
//!
//! ## Expand-only (non-breaking)
//!
//! Three new tables and one new enum type; no existing table or column is
//! touched. Schema-only — no user data seeded (`ARCH-EXPAND-CONTRACT-1`).
//!
//! ## Why raw SQL / idempotency
//!
//! Same rationale as m0001..m0004 (`PROC-CI-MIGRATION-HYGIENE-1`): a guarded
//! `CREATE TYPE` (Postgres has no `CREATE TYPE IF NOT EXISTS`),
//! `CREATE TABLE IF NOT EXISTS`, named FK constraints guarded by a
//! `duplicate_object` catch, and `CREATE INDEX IF NOT EXISTS` read clearest as
//! raw DDL. `up` is safe to re-run; `down` drops in reverse dependency order
//! with `IF EXISTS`.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(PORTFOLIO_INSIGHTS_UP_DDL)
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(PORTFOLIO_INSIGHTS_DOWN_DDL)
            .await?;
        Ok(())
    }
}

/// The expand DDL. Order respects FK dependencies: the `review_terminal_state`
/// enum first, then the three tables (each `IF NOT EXISTS`), each followed by its
/// named FK (guarded by a `duplicate_object` catch — `ADD CONSTRAINT` has no
/// `IF NOT EXISTS` in stable Postgres), its FK index (`SQL-DB-INDEX-1`), and its
/// uniqueness / lookup indexes (`SQL-DB-INDEX-2`).
const PORTFOLIO_INSIGHTS_UP_DDL: &str = r"
-- ============== review_terminal_state pg-enum (ENTITIES-12) ============
-- Mirrors budget_entities::review_runs::ReviewTerminalStateEntity. Guarded:
-- CREATE TYPE has no IF NOT EXISTS, so a re-run swallows the duplicate
-- (PROC-CI-MIGRATION-HYGIENE-1).
DO $$
BEGIN
    CREATE TYPE review_terminal_state AS ENUM
        ('completed', 'no_verifiable_insights', 'empty_portfolio', 'malformed_output');
EXCEPTION
    WHEN duplicate_object THEN NULL;
END
$$;

-- ============================ positions ===============================
-- Investment holdings (AI Portfolio Insights, §1.E/§1.F). shares is a COUNT
-- (NUMERIC, not money); cost_basis is nullable money (NUMERIC, BUDGET-MONEY-1).
-- account_type reuses the account_type pg-enum created in m0001.
CREATE TABLE IF NOT EXISTS positions (
    id            UUID         PRIMARY KEY,
    user_id       UUID         NOT NULL,
    ticker        TEXT         NOT NULL,
    account_label TEXT         NOT NULL,
    account_type  account_type NOT NULL,
    shares        NUMERIC      NOT NULL,
    cost_basis    NUMERIC,
    created_at    TIMESTAMPTZ  NOT NULL,
    updated_at    TIMESTAMPTZ  NOT NULL
);

-- Named FK so `down` can drop it deterministically; guarded (ADD CONSTRAINT has
-- no IF NOT EXISTS). ON DELETE CASCADE: a user's positions go with the user.
DO $$
BEGIN
    ALTER TABLE positions
        ADD CONSTRAINT fk_positions_user_id
        FOREIGN KEY (user_id)
        REFERENCES users (id)
        ON DELETE CASCADE;
EXCEPTION
    WHEN duplicate_object THEN NULL;
END
$$;

-- SQL-DB-INDEX-1: index the FK column in the migration that adds it.
CREATE INDEX IF NOT EXISTS ix_positions_user_id ON positions (user_id);

-- One holding per (user, ticker, account) — the upsert key (ENTITIES-6/7).
CREATE UNIQUE INDEX IF NOT EXISTS uq_positions_user_ticker_account
    ON positions (user_id, ticker, account_label);

-- ============================ cash_balances ===========================
-- Labelled cash balances (§1.E/§1.F). balance is a stock (BUDGET-CASH-1);
-- reserved marks a non-investable buffer.
CREATE TABLE IF NOT EXISTS cash_balances (
    id            UUID        PRIMARY KEY,
    user_id       UUID        NOT NULL,
    account_label TEXT        NOT NULL,
    balance       NUMERIC     NOT NULL,
    reserved      BOOLEAN     NOT NULL DEFAULT false,
    created_at    TIMESTAMPTZ NOT NULL,
    updated_at    TIMESTAMPTZ NOT NULL
);

DO $$
BEGIN
    ALTER TABLE cash_balances
        ADD CONSTRAINT fk_cash_balances_user_id
        FOREIGN KEY (user_id)
        REFERENCES users (id)
        ON DELETE CASCADE;
EXCEPTION
    WHEN duplicate_object THEN NULL;
END
$$;

-- SQL-DB-INDEX-1: FK column index.
CREATE INDEX IF NOT EXISTS ix_cash_balances_user_id ON cash_balances (user_id);

-- One balance per (user, account label) — the upsert key (ENTITIES-6/7).
CREATE UNIQUE INDEX IF NOT EXISTS uq_cash_balances_user_account
    ON cash_balances (user_id, account_label);

-- ============================ review_runs =============================
-- Append-only portfolio-review audit log (SQL-AUDIT-COLUMNS-1): no created_by /
-- modified_by, no updated_at. JSONB payloads (snapshot / outcomes /
-- recommendations, §0.4 + addendum). prompt_tokens / completion_tokens /
-- finish_reason nullable (the provider may not report them; finish_reason is the
-- model's stop reason for truncation / safety-stop audit).
CREATE TABLE IF NOT EXISTS review_runs (
    id                UUID                  PRIMARY KEY,
    user_id           UUID                  NOT NULL,
    model_id          TEXT                  NOT NULL,
    prompt_hash       TEXT                  NOT NULL,
    raw_output        TEXT                  NOT NULL,
    snapshot          JSONB                 NOT NULL,
    outcomes          JSONB                 NOT NULL,
    recommendations   JSONB                 NOT NULL,
    terminal_state    review_terminal_state NOT NULL,
    prompt_tokens     BIGINT,
    completion_tokens BIGINT,
    finish_reason     TEXT,
    latency_ms        BIGINT                NOT NULL,
    occurred_at       TIMESTAMPTZ           NOT NULL
);

DO $$
BEGIN
    ALTER TABLE review_runs
        ADD CONSTRAINT fk_review_runs_user_id
        FOREIGN KEY (user_id)
        REFERENCES users (id)
        ON DELETE CASCADE;
EXCEPTION
    WHEN duplicate_object THEN NULL;
END
$$;

-- SQL-DB-INDEX-1: FK column index.
CREATE INDEX IF NOT EXISTS ix_review_runs_user_id ON review_runs (user_id);

-- SQL-DB-INDEX-2: the history read is 'a user's runs, newest first'.
CREATE INDEX IF NOT EXISTS ix_review_runs_user_occurred
    ON review_runs (user_id, occurred_at DESC);
";

/// The contract DDL: reverse dependency order — each table's indexes → its FK
/// constraint → the table itself (review_runs, then cash_balances, then
/// positions), then `DROP TYPE review_terminal_state` last. Every drop is
/// `IF EXISTS` (`PROC-CI-MIGRATION-HYGIENE-1`).
const PORTFOLIO_INSIGHTS_DOWN_DDL: &str = r"
-- review_runs (no dependents) first.
DROP INDEX IF EXISTS ix_review_runs_user_occurred;
DROP INDEX IF EXISTS ix_review_runs_user_id;
ALTER TABLE IF EXISTS review_runs DROP CONSTRAINT IF EXISTS fk_review_runs_user_id;
DROP TABLE IF EXISTS review_runs;

-- cash_balances next.
DROP INDEX IF EXISTS uq_cash_balances_user_account;
DROP INDEX IF EXISTS ix_cash_balances_user_id;
ALTER TABLE IF EXISTS cash_balances DROP CONSTRAINT IF EXISTS fk_cash_balances_user_id;
DROP TABLE IF EXISTS cash_balances;

-- positions next.
DROP INDEX IF EXISTS uq_positions_user_ticker_account;
DROP INDEX IF EXISTS ix_positions_user_id;
ALTER TABLE IF EXISTS positions DROP CONSTRAINT IF EXISTS fk_positions_user_id;
DROP TABLE IF EXISTS positions;

-- The enum type last, now unreferenced.
DROP TYPE IF EXISTS review_terminal_state;
";

#[cfg(test)]
mod tests {
    //! DB-free structural assertions over the m0007 DDL (`ORCH-NEW-PATH-TESTS-1`),
    //! mirroring the m0004 structural test style. The live apply/down round-trip
    //! is exercised by the `DATABASE_URL`-gated migration integration test.

    use super::{PORTFOLIO_INSIGHTS_DOWN_DDL, PORTFOLIO_INSIGHTS_UP_DDL};

    #[test]
    fn up_creates_review_terminal_state_enum_guarded_with_all_four_variants() {
        assert!(
            PORTFOLIO_INSIGHTS_UP_DDL.contains(
                "CREATE TYPE review_terminal_state AS ENUM\n        ('completed', \
                 'no_verifiable_insights', 'empty_portfolio', 'malformed_output')"
            ),
            "review_terminal_state enum must be created with all four variants"
        );
        assert!(
            PORTFOLIO_INSIGHTS_UP_DDL.contains("WHEN duplicate_object THEN NULL"),
            "CREATE TYPE must be guarded (Postgres has no CREATE TYPE IF NOT EXISTS)"
        );
    }

    #[test]
    fn up_creates_all_three_tables_if_not_exists() {
        for table in ["positions", "cash_balances", "review_runs"] {
            assert!(
                PORTFOLIO_INSIGHTS_UP_DDL.contains(&format!("CREATE TABLE IF NOT EXISTS {table}")),
                "table {table} must be created with IF NOT EXISTS"
            );
        }
    }

    #[test]
    fn up_positions_columns_nullable_cost_basis_unique_and_fk_index() {
        // account_type reuses the existing pg-enum (not a redeclared type).
        assert!(
            PORTFOLIO_INSIGHTS_UP_DDL.contains("account_type  account_type NOT NULL"),
            "positions.account_type must reuse the account_type pg-enum"
        );
        // shares is a COUNT (NUMERIC, not money), cost_basis nullable money.
        assert!(
            PORTFOLIO_INSIGHTS_UP_DDL.contains("shares        NUMERIC      NOT NULL"),
            "positions.shares must be NUMERIC NOT NULL"
        );
        assert!(
            PORTFOLIO_INSIGHTS_UP_DDL.contains("cost_basis    NUMERIC,"),
            "positions.cost_basis must be nullable NUMERIC"
        );
        assert!(
            PORTFOLIO_INSIGHTS_UP_DDL.contains(
                "CREATE UNIQUE INDEX IF NOT EXISTS uq_positions_user_ticker_account\n    \
                 ON positions (user_id, ticker, account_label)"
            ),
            "positions must have the (user_id, ticker, account_label) composite unique"
        );
        assert!(
            PORTFOLIO_INSIGHTS_UP_DDL
                .contains("CREATE INDEX IF NOT EXISTS ix_positions_user_id ON positions (user_id)"),
            "positions.user_id FK column must be indexed in this migration (SQL-DB-INDEX-1)"
        );
        assert!(
            PORTFOLIO_INSIGHTS_UP_DDL.contains("REFERENCES users (id)\n        ON DELETE CASCADE"),
            "positions FK must reference users(id) ON DELETE CASCADE"
        );
    }

    #[test]
    fn up_cash_balances_reserved_default_false_unique_and_fk_index() {
        assert!(
            PORTFOLIO_INSIGHTS_UP_DDL.contains("reserved      BOOLEAN     NOT NULL DEFAULT false"),
            "cash_balances.reserved must be BOOLEAN NOT NULL DEFAULT false"
        );
        assert!(
            PORTFOLIO_INSIGHTS_UP_DDL.contains(
                "CREATE UNIQUE INDEX IF NOT EXISTS uq_cash_balances_user_account\n    \
                 ON cash_balances (user_id, account_label)"
            ),
            "cash_balances must have the (user_id, account_label) composite unique"
        );
        assert!(
            PORTFOLIO_INSIGHTS_UP_DDL.contains(
                "CREATE INDEX IF NOT EXISTS ix_cash_balances_user_id ON cash_balances (user_id)"
            ),
            "cash_balances.user_id FK column must be indexed (SQL-DB-INDEX-1)"
        );
    }

    #[test]
    fn up_review_runs_audit_columns_jsonb_nullable_tokens_no_updated_at() {
        // All JSONB payloads incl. the §0.4-addendum recommendations column.
        for col in [
            "snapshot          JSONB                 NOT NULL",
            "outcomes          JSONB                 NOT NULL",
            "recommendations   JSONB                 NOT NULL",
        ] {
            assert!(
                PORTFOLIO_INSIGHTS_UP_DDL.contains(col),
                "review_runs must have JSONB NOT NULL column: {col}"
            );
        }
        // Token columns are nullable (no NOT NULL).
        assert!(
            PORTFOLIO_INSIGHTS_UP_DDL.contains("prompt_tokens     BIGINT,")
                && PORTFOLIO_INSIGHTS_UP_DDL.contains("completion_tokens BIGINT,"),
            "review_runs token columns must be nullable BIGINT"
        );
        // finish_reason is a nullable TEXT audit column (model stop reason).
        assert!(
            PORTFOLIO_INSIGHTS_UP_DDL.contains("finish_reason     TEXT,"),
            "review_runs.finish_reason must be a nullable TEXT column"
        );
        // latency_ms is NOT NULL.
        assert!(
            PORTFOLIO_INSIGHTS_UP_DDL.contains("latency_ms        BIGINT                NOT NULL"),
            "review_runs.latency_ms must be BIGINT NOT NULL"
        );
        // Append-only audit log: NO updated_at column anywhere in the table.
        // (occurred_at is the single audit timestamp.)
        let review_runs_ddl = PORTFOLIO_INSIGHTS_UP_DDL
            .split("CREATE TABLE IF NOT EXISTS review_runs")
            .nth(1)
            .unwrap_or("");
        let review_runs_block = review_runs_ddl
            .split("review_runs (user_id")
            .next()
            .unwrap_or("");
        assert!(
            !review_runs_block.contains("updated_at"),
            "review_runs must NOT have an updated_at column (append-only, SQL-AUDIT-COLUMNS-1)"
        );
        assert!(
            PORTFOLIO_INSIGHTS_UP_DDL.contains("occurred_at       TIMESTAMPTZ           NOT NULL"),
            "review_runs.occurred_at must be TIMESTAMPTZ NOT NULL"
        );
    }

    #[test]
    fn up_review_runs_fk_index_and_history_index() {
        assert!(
            PORTFOLIO_INSIGHTS_UP_DDL.contains(
                "CREATE INDEX IF NOT EXISTS ix_review_runs_user_id ON review_runs (user_id)"
            ),
            "review_runs.user_id FK column must be indexed (SQL-DB-INDEX-1)"
        );
        assert!(
            PORTFOLIO_INSIGHTS_UP_DDL.contains(
                "CREATE INDEX IF NOT EXISTS ix_review_runs_user_occurred\n    \
                 ON review_runs (user_id, occurred_at DESC)"
            ),
            "review_runs must have the (user_id, occurred_at DESC) history index (SQL-DB-INDEX-2)"
        );
    }

    #[test]
    fn down_drops_in_dependency_order_review_runs_before_enum() {
        // Each table's drop precedes the enum drop; tables drop in reverse order
        // (review_runs, cash_balances, positions); the enum drops last. Compare
        // Option<usize> positions directly to avoid unwrap (clippy denies it).
        let review_runs = PORTFOLIO_INSIGHTS_DOWN_DDL.find("DROP TABLE IF EXISTS review_runs");
        let cash_balances = PORTFOLIO_INSIGHTS_DOWN_DDL.find("DROP TABLE IF EXISTS cash_balances");
        let positions = PORTFOLIO_INSIGHTS_DOWN_DDL.find("DROP TABLE IF EXISTS positions");
        let enum_drop =
            PORTFOLIO_INSIGHTS_DOWN_DDL.find("DROP TYPE IF EXISTS review_terminal_state");
        assert!(review_runs.is_some(), "down must drop review_runs");
        assert!(cash_balances.is_some(), "down must drop cash_balances");
        assert!(positions.is_some(), "down must drop positions");
        assert!(enum_drop.is_some(), "down must drop the enum type");
        assert!(
            review_runs < cash_balances,
            "review_runs must drop before cash_balances"
        );
        assert!(
            cash_balances < positions,
            "cash_balances must drop before positions"
        );
        assert!(
            positions < enum_drop,
            "all tables must drop before the enum type"
        );
    }

    #[test]
    fn down_drops_every_index_and_table_if_exists() {
        for stmt in [
            "DROP INDEX IF EXISTS ix_review_runs_user_occurred",
            "DROP INDEX IF EXISTS ix_review_runs_user_id",
            "DROP INDEX IF EXISTS uq_cash_balances_user_account",
            "DROP INDEX IF EXISTS ix_cash_balances_user_id",
            "DROP INDEX IF EXISTS uq_positions_user_ticker_account",
            "DROP INDEX IF EXISTS ix_positions_user_id",
            "DROP TABLE IF EXISTS review_runs",
            "DROP TABLE IF EXISTS cash_balances",
            "DROP TABLE IF EXISTS positions",
        ] {
            assert!(
                PORTFOLIO_INSIGHTS_DOWN_DDL.contains(stmt),
                "down must contain guarded statement: {stmt}"
            );
        }
    }
}
