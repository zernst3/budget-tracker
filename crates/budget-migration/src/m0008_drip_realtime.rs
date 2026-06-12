//! m0008 — DRIP & real-time position tracking schema (`docs/DRIP_REALTIME_DESIGN.md §4`).
//!
//! Three expand-only changes:
//!
//!   1. `positions` gains `drip_enabled BOOLEAN NOT NULL DEFAULT false` (the
//!      per-position DRIP toggle, persists across uploads §2.7/§6) and
//!      `baseline_as_of TIMESTAMPTZ NOT NULL DEFAULT now()` (the confirmed-baseline
//!      as-of date, `BUDGET-CUTOVER-1`). Both `ADD COLUMN IF NOT EXISTS`. The
//!      `DEFAULT now()` on `baseline_as_of` correctly stamps any pre-existing
//!      position with its migration time as the initial baseline.
//!   2. `dividend_events` — a NEW ticker-keyed dividend cache (shared across
//!      positions of the same ticker). `amount_per_share` NUMERIC money
//!      (`BUDGET-MONEY-1`); `source` provenance TEXT; unique `(ticker, pay_date)`
//!      + a `ticker` lookup index. NO user FK (the cache is global per ticker).
//!   3. `drip_applications` — a NEW append-only DRIP accretion chain
//!      (`SQL-AUDIT-COLUMNS-1`): FK→positions (Cascade); unique
//!      `(position_id, pay_date)` (the idempotency guard, §6); FK index on
//!      `position_id` + a `(user_id, applied_at)` history index. `shares_added` a
//!      COUNT, `cash_added`/`amount_per_share`/`price_used` money — all NUMERIC.
//!      No `created_by`/`updated_at` (append-only system log).
//!
//! ## Why a new migration (not an m0007 edit)
//!
//! m0007 is a shipped migration; per `ARCH-EXPAND-CONTRACT-1` /
//! `PROC-CI-MIGRATION-HYGIENE-1` schema changes append as their own migration,
//! never an in-place edit of an already-run one (the journal keys off the name).
//!
//! ## Expand-only (non-breaking)
//!
//! Two additive columns (both with safe defaults for existing rows) and two new
//! tables; no existing column is dropped or made stricter. Schema-only — no user
//! data seeded (`ARCH-EXPAND-CONTRACT-1`).
//!
//! ## Why raw SQL / idempotency
//!
//! Same rationale as m0001..m0007 (`PROC-CI-MIGRATION-HYGIENE-1`): `ADD COLUMN IF
//! NOT EXISTS`, `CREATE TABLE IF NOT EXISTS`, named FK constraints guarded by a
//! `duplicate_object` catch, and `CREATE INDEX IF NOT EXISTS` read clearest as raw
//! DDL. `up` is safe to re-run; `down` drops in reverse dependency order with
//! `IF EXISTS` and drops the two added columns.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(DRIP_REALTIME_UP_DDL)
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(DRIP_REALTIME_DOWN_DDL)
            .await?;
        Ok(())
    }
}

/// The expand DDL. `positions` columns first (additive), then the two new tables
/// (each `IF NOT EXISTS`), each followed by its named FK (guarded by a
/// `duplicate_object` catch — `ADD CONSTRAINT` has no `IF NOT EXISTS`), its FK
/// index (`SQL-DB-INDEX-1`), and its uniqueness / lookup indexes (`SQL-DB-INDEX-2`).
const DRIP_REALTIME_UP_DDL: &str = r"
-- ====================== positions: DRIP columns =======================
-- drip_enabled: the per-position toggle. NOT NULL DEFAULT false — DRIP is
-- opt-in, and all existing rows are correctly initialized to 'off'. PERSISTS
-- across uploads for surviving positions (§2.7/§6).
ALTER TABLE positions
    ADD COLUMN IF NOT EXISTS drip_enabled BOOLEAN NOT NULL DEFAULT false;

-- baseline_as_of: the confirmed-baseline as-of date (BUDGET-CUTOVER-1). NOT NULL
-- DEFAULT now() — any pre-existing position is correctly stamped with its
-- migration time as the initial baseline; uploads overwrite it with the upload
-- date. DRIP applies only to dividend events with pay_date > baseline_as_of.
ALTER TABLE positions
    ADD COLUMN IF NOT EXISTS baseline_as_of TIMESTAMPTZ NOT NULL DEFAULT now();

-- ========================= dividend_events ============================
-- A ticker-keyed dividend cache, SHARED across positions of the same ticker so a
-- dividend is fetched once (§4). amount_per_share is NUMERIC money
-- (BUDGET-MONEY-1); source is the chain-tier provenance (tiingo/yahoo/manual/
-- mock). NO user FK — the cache is global per ticker.
CREATE TABLE IF NOT EXISTS dividend_events (
    id               UUID        PRIMARY KEY,
    ticker           TEXT        NOT NULL,
    ex_date          DATE        NOT NULL,
    pay_date         DATE        NOT NULL,
    amount_per_share NUMERIC     NOT NULL,
    source           TEXT        NOT NULL,
    fetched_at       TIMESTAMPTZ NOT NULL
);

-- One cache row per dividend (ticker, pay_date) — the upsert key (§4).
CREATE UNIQUE INDEX IF NOT EXISTS uq_dividend_events_ticker_pay_date
    ON dividend_events (ticker, pay_date);

-- SQL-DB-INDEX-2: the read is 'this ticker's dividends since a cutoff'.
CREATE INDEX IF NOT EXISTS ix_dividend_events_ticker ON dividend_events (ticker);

-- ========================= drip_applications ==========================
-- The append-only DRIP accretion chain (SQL-AUDIT-COLUMNS-1,
-- BUDGET-ROLLOVER-INTEGRITY-1): no created_by / modified_by, no updated_at.
-- shares_added a COUNT; cash_added / amount_per_share / price_used money — all
-- NUMERIC (BUDGET-MONEY-1). drip_on_at_apply records the toggle at apply time.
CREATE TABLE IF NOT EXISTS drip_applications (
    id               UUID        PRIMARY KEY,
    user_id          UUID        NOT NULL,
    position_id      UUID        NOT NULL,
    ticker           TEXT        NOT NULL,
    pay_date         DATE        NOT NULL,
    amount_per_share NUMERIC     NOT NULL,
    price_used       NUMERIC     NOT NULL,
    shares_added     NUMERIC     NOT NULL,
    cash_added       NUMERIC     NOT NULL,
    drip_on_at_apply BOOLEAN     NOT NULL,
    applied_at       TIMESTAMPTZ NOT NULL
);

-- Named FK so `down` can drop it deterministically; guarded (ADD CONSTRAINT has
-- no IF NOT EXISTS). ON DELETE CASCADE: a position's chain goes with the position.
DO $$
BEGIN
    ALTER TABLE drip_applications
        ADD CONSTRAINT fk_drip_applications_position_id
        FOREIGN KEY (position_id)
        REFERENCES positions (id)
        ON DELETE CASCADE;
EXCEPTION
    WHEN duplicate_object THEN NULL;
END
$$;

-- The idempotency guard: one application per (position, dividend pay-date), §6.
CREATE UNIQUE INDEX IF NOT EXISTS uq_drip_applications_position_pay_date
    ON drip_applications (position_id, pay_date);

-- SQL-DB-INDEX-1: index the FK column in the migration that adds it.
CREATE INDEX IF NOT EXISTS ix_drip_applications_position_id
    ON drip_applications (position_id);

-- SQL-DB-INDEX-2: a user's applications, newest first (audit read).
CREATE INDEX IF NOT EXISTS ix_drip_applications_user_applied
    ON drip_applications (user_id, applied_at DESC);
";

/// The contract DDL: reverse dependency order — `drip_applications` (its indexes
/// → FK → table), then `dividend_events` (indexes → table), then drop the two
/// `positions` columns. Every drop is `IF EXISTS` (`PROC-CI-MIGRATION-HYGIENE-1`).
const DRIP_REALTIME_DOWN_DDL: &str = r"
-- drip_applications first (depends on positions).
DROP INDEX IF EXISTS ix_drip_applications_user_applied;
DROP INDEX IF EXISTS ix_drip_applications_position_id;
DROP INDEX IF EXISTS uq_drip_applications_position_pay_date;
ALTER TABLE IF EXISTS drip_applications
    DROP CONSTRAINT IF EXISTS fk_drip_applications_position_id;
DROP TABLE IF EXISTS drip_applications;

-- dividend_events next (no dependents).
DROP INDEX IF EXISTS ix_dividend_events_ticker;
DROP INDEX IF EXISTS uq_dividend_events_ticker_pay_date;
DROP TABLE IF EXISTS dividend_events;

-- positions columns last.
ALTER TABLE positions DROP COLUMN IF EXISTS baseline_as_of;
ALTER TABLE positions DROP COLUMN IF EXISTS drip_enabled;
";

#[cfg(test)]
mod tests {
    //! DB-free structural assertions over the m0008 DDL (`ORCH-NEW-PATH-TESTS-1`),
    //! mirroring the m0006/m0007 structural test style. The live apply/down
    //! round-trip is exercised by the `DATABASE_URL`-gated migration integration
    //! test.

    use super::{DRIP_REALTIME_DOWN_DDL, DRIP_REALTIME_UP_DDL};

    #[test]
    fn up_adds_positions_drip_columns_idempotently() {
        assert!(
            DRIP_REALTIME_UP_DDL
                .contains("ADD COLUMN IF NOT EXISTS drip_enabled BOOLEAN NOT NULL DEFAULT false"),
            "positions.drip_enabled must be BOOLEAN NOT NULL DEFAULT false with IF NOT EXISTS"
        );
        assert!(
            DRIP_REALTIME_UP_DDL.contains(
                "ADD COLUMN IF NOT EXISTS baseline_as_of TIMESTAMPTZ NOT NULL DEFAULT now()"
            ),
            "positions.baseline_as_of must be TIMESTAMPTZ NOT NULL DEFAULT now() with IF NOT EXISTS"
        );
    }

    #[test]
    fn up_creates_both_new_tables_if_not_exists() {
        for table in ["dividend_events", "drip_applications"] {
            assert!(
                DRIP_REALTIME_UP_DDL.contains(&format!("CREATE TABLE IF NOT EXISTS {table}")),
                "table {table} must be created with IF NOT EXISTS"
            );
        }
    }

    #[test]
    fn up_dividend_events_money_unique_and_lookup_index_no_user_fk() {
        assert!(
            DRIP_REALTIME_UP_DDL.contains("amount_per_share NUMERIC     NOT NULL"),
            "dividend_events.amount_per_share must be NUMERIC NOT NULL (money)"
        );
        assert!(
            DRIP_REALTIME_UP_DDL.contains(
                "CREATE UNIQUE INDEX IF NOT EXISTS uq_dividend_events_ticker_pay_date\n    \
                 ON dividend_events (ticker, pay_date)"
            ),
            "dividend_events must have the (ticker, pay_date) composite unique"
        );
        assert!(
            DRIP_REALTIME_UP_DDL.contains(
                "CREATE INDEX IF NOT EXISTS ix_dividend_events_ticker ON dividend_events (ticker)"
            ),
            "dividend_events must have a ticker lookup index (SQL-DB-INDEX-2)"
        );
        // The cache is global per ticker — no user FK.
        assert!(
            !DRIP_REALTIME_UP_DDL.contains("fk_dividend_events"),
            "dividend_events must not declare a user FK (global per-ticker cache)"
        );
    }

    #[test]
    fn up_drip_applications_idempotency_guard_fk_and_history_index() {
        // The (position_id, pay_date) idempotency guard (§6).
        assert!(
            DRIP_REALTIME_UP_DDL.contains(
                "CREATE UNIQUE INDEX IF NOT EXISTS uq_drip_applications_position_pay_date\n    \
                 ON drip_applications (position_id, pay_date)"
            ),
            "drip_applications must have the (position_id, pay_date) idempotency unique"
        );
        // FK index on position_id, in this migration (SQL-DB-INDEX-1).
        assert!(
            DRIP_REALTIME_UP_DDL.contains(
                "CREATE INDEX IF NOT EXISTS ix_drip_applications_position_id\n    \
                 ON drip_applications (position_id)"
            ),
            "drip_applications.position_id FK column must be indexed (SQL-DB-INDEX-1)"
        );
        // (user_id, applied_at DESC) history index (SQL-DB-INDEX-2).
        assert!(
            DRIP_REALTIME_UP_DDL.contains(
                "CREATE INDEX IF NOT EXISTS ix_drip_applications_user_applied\n    \
                 ON drip_applications (user_id, applied_at DESC)"
            ),
            "drip_applications must have a (user_id, applied_at DESC) history index"
        );
        // FK references positions ON DELETE CASCADE.
        assert!(
            DRIP_REALTIME_UP_DDL.contains("REFERENCES positions (id)\n        ON DELETE CASCADE"),
            "drip_applications FK must reference positions(id) ON DELETE CASCADE"
        );
        // Append-only: no updated_at column anywhere in the table block.
        let block = DRIP_REALTIME_UP_DDL
            .split("CREATE TABLE IF NOT EXISTS drip_applications")
            .nth(1)
            .and_then(|s| s.split(");").next())
            .unwrap_or("");
        assert!(
            !block.contains("updated_at"),
            "drip_applications must NOT have an updated_at column (append-only)"
        );
    }

    #[test]
    fn down_drops_in_dependency_order_applications_before_positions_columns() {
        let applications = DRIP_REALTIME_DOWN_DDL.find("DROP TABLE IF EXISTS drip_applications");
        let events = DRIP_REALTIME_DOWN_DDL.find("DROP TABLE IF EXISTS dividend_events");
        let baseline_col = DRIP_REALTIME_DOWN_DDL.find("DROP COLUMN IF EXISTS baseline_as_of");
        assert!(applications.is_some(), "down must drop drip_applications");
        assert!(events.is_some(), "down must drop dividend_events");
        assert!(
            baseline_col.is_some(),
            "down must drop baseline_as_of column"
        );
        assert!(
            applications < events,
            "drip_applications (FK→positions) must drop before dividend_events"
        );
        assert!(
            events < baseline_col,
            "tables must drop before the positions columns"
        );
    }

    #[test]
    fn down_drops_every_index_table_and_column_if_exists() {
        for stmt in [
            "DROP INDEX IF EXISTS ix_drip_applications_user_applied",
            "DROP INDEX IF EXISTS ix_drip_applications_position_id",
            "DROP INDEX IF EXISTS uq_drip_applications_position_pay_date",
            "DROP TABLE IF EXISTS drip_applications",
            "DROP INDEX IF EXISTS ix_dividend_events_ticker",
            "DROP INDEX IF EXISTS uq_dividend_events_ticker_pay_date",
            "DROP TABLE IF EXISTS dividend_events",
            "DROP COLUMN IF EXISTS baseline_as_of",
            "DROP COLUMN IF EXISTS drip_enabled",
        ] {
            assert!(
                DRIP_REALTIME_DOWN_DDL.contains(stmt),
                "down must contain guarded statement: {stmt}"
            );
        }
    }
}
