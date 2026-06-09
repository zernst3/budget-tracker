//! m0001 — genesis schema.
//!
//! Creates, in one atomic step: the 13 Postgres `ENUM` types (`ENTITIES-12`),
//! all ten `SPEC` §5 tables, the foreign-key indexes (`SQL-DB-INDEX-1`), the
//! repository lookup indexes (`SQL-DB-INDEX-2`), and the four `SPEC` §12 DB
//! constraints (`BUDGET-ROLLOVER-INTEGRITY-1`, `BUDGET-IDEMPOTENT-MONTH-INIT-1`,
//! Plaid dedup).
//!
//! This is the genesis create, so `ARCH-EXPAND-CONTRACT-1` has no prior shape to
//! expand from; later breaking changes must use the expand→migrate→contract
//! split in their own migrations. Schema-only: no user data is seeded
//! (`ARCH-EXPAND-CONTRACT-1`).
//!
//! ## Why raw SQL
//!
//! The schema leans on three Postgres features the `SeaORM` type-safe builder does
//! not express cleanly: real `ENUM` types (`ENTITIES-12`), `NUMERIC(18,2)` money
//! columns (`BUDGET-MONEY-1`), and **partial** unique indexes with a `WHERE`
//! clause (the two §12 rollover constraints). Authoring the whole genesis as raw
//! SQL keeps every column type, default, and constraint explicit and lets every
//! statement carry `IF NOT EXISTS` / `DROP ... IF EXISTS` for idempotency
//! (`PROC-CI-MIGRATION-HYGIENE-1`). `RUST-SEAORM-RAW-SQL-ESCAPE-1` governs
//! repository **read** methods, not migrations; raw DDL is the idiomatic `SeaORM`
//! path for these Postgres-specific constructs.
//!
//! ## Idempotency
//!
//! `up` is safe to re-run: enum types use a `DO $$ ... EXCEPTION WHEN
//! duplicate_object` guard (Postgres has no `CREATE TYPE IF NOT EXISTS`), every
//! `CREATE TABLE` and `CREATE INDEX` uses `IF NOT EXISTS`. `down` drops in
//! reverse dependency order with `IF EXISTS`.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

/// Each pg-enum type, paired with its `CREATE TYPE` body. Mirrors the
/// `DeriveActiveEnum` `enum_name` + `string_value`s in `budget-entities`
/// (`ENTITIES-12`). Postgres has no `CREATE TYPE IF NOT EXISTS`, so each is
/// wrapped in a `DO` block that swallows `duplicate_object` for idempotency.
const ENUM_TYPES: &[(&str, &str)] = &[
    ("category_grp", "'fixed', 'discretionary'"),
    ("settle_type", "'true_set', 'flexible_set'"),
    ("cadence", "'monthly', 'quarterly', 'semiannual', 'annual'"),
    (
        "account_type",
        "'checking', 'credit', 'savings', 'investment', 'other'",
    ),
    ("month_status", "'open', 'closed'"),
    ("transaction_source", "'manual', 'plaid'"),
    ("transaction_status", "'pending', 'settled', 'expected'"),
    ("income_kind", "'budgeted', 'new'"),
    ("fund_kind", "'buffer', 'surplus'"),
    ("obligation_status", "'active', 'paid'"),
    ("income_mode", "'per_paycheck', 'smoothed'"),
    (
        "paycheck_type",
        "'semimonthly', 'biweekly', 'weekly', 'hourly'",
    ),
    ("surplus_routing", "'buffer', 'this_month', 'savings'"),
];

/// Tables in reverse dependency order, for the `down` teardown. (Listing them
/// once keeps `down` in sync with `up` without restating the full DDL.)
const TABLES_REVERSE_DROP_ORDER: &[&str] = &[
    "repayment_obligations",
    "transactions",
    "months",
    "accounts",
    "plaid_items",
    "categories",
    "funds",
    "paycheck_config",
    "budgets",
    "users",
];

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        // ---- 1. Postgres ENUM types (ENTITIES-12) -------------------------
        // Idempotent: CREATE TYPE has no IF NOT EXISTS, so each is guarded.
        for (name, variants) in ENUM_TYPES {
            db.execute_unprepared(&format!(
                "DO $$ BEGIN
                     CREATE TYPE {name} AS ENUM ({variants});
                 EXCEPTION
                     WHEN duplicate_object THEN NULL;
                 END $$;"
            ))
            .await?;
        }

        // ---- 2. Tables (SPEC §5) + adjacent FK / lookup indexes -----------
        // Each statement is IF NOT EXISTS for re-run safety
        // (PROC-CI-MIGRATION-HYGIENE-1). FK indexes are declared right after the
        // table that owns the FK (SQL-DB-INDEX-1); lookup indexes supporting the
        // repository WHERE/ORDER columns follow (SQL-DB-INDEX-2).
        db.execute_unprepared(GENESIS_DDL).await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        // Drop tables in reverse dependency order (children before parents).
        // CASCADE clears the dependent indexes/constraints with the table.
        for table in TABLES_REVERSE_DROP_ORDER {
            db.execute_unprepared(&format!("DROP TABLE IF EXISTS {table} CASCADE;"))
                .await?;
        }

        // Then drop the enum types (now unreferenced).
        for (name, _) in ENUM_TYPES {
            db.execute_unprepared(&format!("DROP TYPE IF EXISTS {name};"))
                .await?;
        }

        Ok(())
    }
}

/// The full genesis DDL. Ordering respects FK dependencies: `users` first, then
/// budget / paycheck / funds, then `categories` / `plaid_items`, then `accounts`,
/// `months`, `transactions`, and finally `repayment_obligations`.
///
/// Column types mirror `budget-entities` exactly:
///   - UUID PKs (`auto_increment = false`): `UUID` (app-generated, `DOMAIN-2/9`).
///   - money columns: `NUMERIC(18,2)` (`BUDGET-MONEY-1`).
///   - timestamps: `TIMESTAMPTZ` (`ARCH-UTC-TIMESTAMPS-1`); dates: `DATE`.
///   - enum columns: the real pg `ENUM` types created above (`ENTITIES-12`).
const GENESIS_DDL: &str = r"
-- ============================ users ===================================
-- Sole user (V1); every core table carries user_id for future-proofing (SPEC §5).
-- tracking_start_date is the genesis cutover boundary (BUDGET-CUTOVER-1, §12 D8).
CREATE TABLE IF NOT EXISTS users (
    id                  UUID        PRIMARY KEY,
    email               TEXT        NOT NULL,
    password_hash       TEXT        NOT NULL,
    totp_secret         TEXT,
    tracking_start_date DATE        NOT NULL,
    created_at          TIMESTAMPTZ NOT NULL
);
-- Single user logs in by email; unique + lookup support for the auth path.
CREATE UNIQUE INDEX IF NOT EXISTS ux_users_email ON users (email);

-- ============================ budgets =================================
-- Versioned config; months REFERENCE a version by FK (SPEC §4.1).
-- effective_to IS NULL = the current active version.
CREATE TABLE IF NOT EXISTS budgets (
    id             UUID        PRIMARY KEY,
    user_id        UUID        NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    name           TEXT        NOT NULL,
    effective_from DATE        NOT NULL,
    effective_to   DATE,
    created_at     TIMESTAMPTZ NOT NULL
);
-- DB-INDEX-1: FK column index.
CREATE INDEX IF NOT EXISTS ix_budgets_user_id ON budgets (user_id);
-- DB-INDEX-2: 'current active version' lookup = user_id WHERE effective_to IS NULL.
CREATE INDEX IF NOT EXISTS ix_budgets_user_active
    ON budgets (user_id) WHERE effective_to IS NULL;

-- ============================ paycheck_config =========================
-- Income setup; one per user (SPEC §4.8). All mode fields first-class; only the
-- semimonthly per_paycheck path is actively built in V1 (rest stubbed).
CREATE TABLE IF NOT EXISTS paycheck_config (
    id               UUID            PRIMARY KEY,
    user_id          UUID            NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    income_mode      income_mode     NOT NULL,
    paycheck_type    paycheck_type   NOT NULL,
    amount           NUMERIC(18, 2),
    anchor_date      DATE            NOT NULL,
    surplus_routing  surplus_routing NOT NULL,
    smoothing_buffer NUMERIC(18, 2)  NOT NULL DEFAULT 0
);
-- DB-INDEX-1: FK column index. One row per user (find_for_user lookup).
CREATE UNIQUE INDEX IF NOT EXISTS ux_paycheck_config_user_id ON paycheck_config (user_id);

-- ============================ funds ===================================
-- Buffer / surplus virtual envelopes (SPEC §4.9). compulsory_repayment = true
-- for buffer, false for surplus.
CREATE TABLE IF NOT EXISTS funds (
    id                   UUID        PRIMARY KEY,
    user_id              UUID        NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    name                 TEXT        NOT NULL,
    kind                 fund_kind   NOT NULL,
    balance              NUMERIC(18, 2) NOT NULL DEFAULT 0,
    target_balance       NUMERIC(18, 2),
    compulsory_repayment BOOLEAN     NOT NULL,
    created_at           TIMESTAMPTZ NOT NULL
);
-- DB-INDEX-1: FK column index.
CREATE INDEX IF NOT EXISTS ix_funds_user_id ON funds (user_id);

-- ============================ categories ==============================
-- Buckets within a budget version (SPEC §4.2). cadence > monthly = sinking fund.
-- category_key = stable lineage across budget versions (D3, §12).
CREATE TABLE IF NOT EXISTS categories (
    id                 UUID         PRIMARY KEY,
    budget_id          UUID         NOT NULL REFERENCES budgets (id) ON DELETE CASCADE,
    category_key       UUID         NOT NULL,
    name               TEXT         NOT NULL,
    amount             NUMERIC(18, 2) NOT NULL,
    grp                category_grp NOT NULL,
    settle_type        settle_type,
    expected_bills     INTEGER,
    is_rollover_bucket BOOLEAN      NOT NULL DEFAULT FALSE,
    cadence            cadence      NOT NULL DEFAULT 'monthly',
    period_months      INTEGER,
    fund_balance       NUMERIC(18, 2) NOT NULL DEFAULT 0,
    next_due_date      DATE,
    sort_order         INTEGER      NOT NULL
);
-- DB-INDEX-1: FK column index.
CREATE INDEX IF NOT EXISTS ix_categories_budget_id ON categories (budget_id);
-- DB-INDEX-2: cross-version lineage lookups by category_key (D3, deferred but indexed).
CREATE INDEX IF NOT EXISTS ix_categories_category_key ON categories (category_key);
-- SPEC §12 D#11 / BUDGET-ROLLOVER-INTEGRITY-1 (ENTITIES-8): exactly ONE rollover
-- bucket per budget version. Partial unique — the SeaORM macro cannot express it.
CREATE UNIQUE INDEX IF NOT EXISTS ux_categories_one_rollover_per_budget
    ON categories (budget_id) WHERE is_rollover_bucket;

-- ============================ plaid_items =============================
-- One per linked institution (SPEC §6). access_token_ref = Key Vault REFERENCE
-- only, never the raw token (BUDGET-PLAID-TOKEN-VAULT-1).
CREATE TABLE IF NOT EXISTS plaid_items (
    id               UUID        PRIMARY KEY,
    user_id          UUID        NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    institution_name TEXT        NOT NULL,
    access_token_ref TEXT        NOT NULL,
    sync_cursor      TEXT,
    last_synced_at   TIMESTAMPTZ,
    created_at       TIMESTAMPTZ NOT NULL
);
-- DB-INDEX-1: FK column index.
CREATE INDEX IF NOT EXISTS ix_plaid_items_user_id ON plaid_items (user_id);

-- ============================ accounts ================================
-- Bank accounts, optionally linked via Plaid (SPEC §5). plaid_item_id nullable.
CREATE TABLE IF NOT EXISTS accounts (
    id               UUID         PRIMARY KEY,
    user_id          UUID         NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    name             TEXT         NOT NULL,
    type             account_type NOT NULL,
    plaid_account_id TEXT,
    plaid_item_id    UUID         REFERENCES plaid_items (id) ON DELETE SET NULL
);
-- DB-INDEX-1: FK column indexes (both FKs).
CREATE INDEX IF NOT EXISTS ix_accounts_user_id ON accounts (user_id);
CREATE INDEX IF NOT EXISTS ix_accounts_plaid_item_id ON accounts (plaid_item_id);

-- ============================ months ==================================
-- open/closed lifecycle; REFERENCES a budget version (SPEC §4.6).
-- Month-membership computed in America/New_York; timestamps UTC (D2, §12).
CREATE TABLE IF NOT EXISTS months (
    id         UUID         PRIMARY KEY,
    user_id    UUID         NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    budget_id  UUID         NOT NULL REFERENCES budgets (id) ON DELETE RESTRICT,
    year       INTEGER      NOT NULL,
    month      INTEGER      NOT NULL,
    status     month_status NOT NULL,
    opened_at  TIMESTAMPTZ  NOT NULL,
    closed_at  TIMESTAMPTZ
);
-- DB-INDEX-1: FK column indexes.
CREATE INDEX IF NOT EXISTS ix_months_user_id ON months (user_id);
CREATE INDEX IF NOT EXISTS ix_months_budget_id ON months (budget_id);
-- SPEC §5 / §12 / BUDGET-IDEMPOTENT-MONTH-INIT-1 (ENTITIES-7): UNIQUE(user_id,
-- year, month). Makes lazy-init re-entry idempotent and doubles as the
-- (user_id, year, month) lookup index for the month-resolution path (DB-INDEX-2).
CREATE UNIQUE INDEX IF NOT EXISTS ux_months_user_year_month
    ON months (user_id, year, month);

-- ============================ transactions ============================
-- Central record type (SPEC §5). amount signed: negative = expense, positive =
-- inflow (BUDGET-PLAID-SIGN-1 flips Plaid at the mapper boundary).
CREATE TABLE IF NOT EXISTS transactions (
    id                   UUID               PRIMARY KEY,
    user_id              UUID               NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    month_id             UUID               NOT NULL REFERENCES months (id) ON DELETE CASCADE,
    category_id          UUID               REFERENCES categories (id) ON DELETE SET NULL,
    account_id           UUID               REFERENCES accounts (id) ON DELETE SET NULL,
    date                 DATE               NOT NULL,
    amount               NUMERIC(18, 2)     NOT NULL,
    description          TEXT               NOT NULL,
    source               transaction_source NOT NULL,
    plaid_transaction_id TEXT,
    status               transaction_status NOT NULL,
    income_kind          income_kind,
    is_rollover          BOOLEAN            NOT NULL DEFAULT FALSE,
    -- D6 Model A (the no-recharge rule): a fund DRAW (surplus draw, sinking payout)
    -- that must NOT be re-charged against the month budget. The cash was already
    -- expensed at contribution time; the draw is excluded from the month
    -- expense-remaining sum. Contributions/installments/accruals leave this FALSE
    -- and therefore COUNT.
    is_fund_draw         BOOLEAN            NOT NULL DEFAULT FALSE,
    created_at           TIMESTAMPTZ        NOT NULL,
    updated_at           TIMESTAMPTZ        NOT NULL
);
-- DB-INDEX-1: FK column indexes (all four FKs).
CREATE INDEX IF NOT EXISTS ix_transactions_user_id ON transactions (user_id);
CREATE INDEX IF NOT EXISTS ix_transactions_month_id ON transactions (month_id);
CREATE INDEX IF NOT EXISTS ix_transactions_category_id ON transactions (category_id);
CREATE INDEX IF NOT EXISTS ix_transactions_account_id ON transactions (account_id);
-- DB-INDEX-2: per-month category aggregation (actual-spent / remaining) filters
-- on (month_id, category_id); the most-selective column (month_id) leads.
CREATE INDEX IF NOT EXISTS ix_transactions_month_category
    ON transactions (month_id, category_id);
-- DB-INDEX-2: date-range scans within a month (statement ordering, reconcile).
CREATE INDEX IF NOT EXISTS ix_transactions_date ON transactions (date);
-- SPEC §5 / §12 / Plaid dedup (ENTITIES-8): plaid_transaction_id UNIQUE. Partial
-- (WHERE NOT NULL) so the many NULL manual rows do not collide; also serves as
-- the dedup lookup index on Plaid sync (DB-INDEX-2).
CREATE UNIQUE INDEX IF NOT EXISTS ux_transactions_plaid_transaction_id
    ON transactions (plaid_transaction_id) WHERE plaid_transaction_id IS NOT NULL;
-- SPEC §12 / BUDGET-ROLLOVER-INTEGRITY-1 (ENTITIES-8): at most ONE rollover line
-- per month. Partial unique — prevents double-posting the 1st-of-month rollover.
CREATE UNIQUE INDEX IF NOT EXISTS ux_transactions_one_rollover_per_month
    ON transactions (month_id) WHERE is_rollover;

-- ============================ repayment_obligations ===================
-- Created when the buffer funds a large purchase (SPEC §4.9 D7). Installments
-- are the compulsory monthly budget expenses until remaining_amount = 0.
CREATE TABLE IF NOT EXISTS repayment_obligations (
    id                 UUID              PRIMARY KEY,
    user_id            UUID              NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    fund_id            UUID              NOT NULL REFERENCES funds (id) ON DELETE RESTRICT,
    transaction_id     UUID              NOT NULL REFERENCES transactions (id) ON DELETE RESTRICT,
    total_amount       NUMERIC(18, 2)    NOT NULL,
    remaining_amount   NUMERIC(18, 2)    NOT NULL,
    installment_amount NUMERIC(18, 2)    NOT NULL,
    months_remaining   INTEGER           NOT NULL,
    status             obligation_status NOT NULL,
    created_at         TIMESTAMPTZ       NOT NULL
);
-- DB-INDEX-1: FK column indexes (all three FKs).
CREATE INDEX IF NOT EXISTS ix_repayment_obligations_user_id ON repayment_obligations (user_id);
CREATE INDEX IF NOT EXISTS ix_repayment_obligations_fund_id ON repayment_obligations (fund_id);
CREATE INDEX IF NOT EXISTS ix_repayment_obligations_transaction_id
    ON repayment_obligations (transaction_id);
-- DB-INDEX-2: 'active obligations for a fund' is the repayment-application query.
CREATE INDEX IF NOT EXISTS ix_repayment_obligations_fund_active
    ON repayment_obligations (fund_id) WHERE status = 'active';
";

#[cfg(test)]
mod tests {
    //! DB-free structural assertions over the genesis DDL. These pin the
    //! invariants the SPEC requires without needing a live Postgres, so they run
    //! in CI unconditionally (`ORCH-NEW-PATH-TESTS-1`). The actual apply / down /
    //! re-apply round-trip against a real Postgres lives in
    //! `tests/genesis_applies.rs`, gated on `DATABASE_URL`.

    use super::{ENUM_TYPES, GENESIS_DDL, TABLES_REVERSE_DROP_ORDER};

    /// All 10 SPEC §5 tables are created.
    #[test]
    fn ddl_creates_all_ten_spec_tables() {
        let tables = [
            "users",
            "budgets",
            "categories",
            "accounts",
            "plaid_items",
            "months",
            "transactions",
            "funds",
            "repayment_obligations",
            "paycheck_config",
        ];
        for t in tables {
            assert!(
                GENESIS_DDL.contains(&format!("CREATE TABLE IF NOT EXISTS {t} (")),
                "missing CREATE TABLE for `{t}`",
            );
        }
    }

    /// `down` drops every table `up` creates — the two lists stay in sync.
    #[test]
    fn down_drops_every_created_table() {
        assert_eq!(
            TABLES_REVERSE_DROP_ORDER.len(),
            10,
            "TABLES_REVERSE_DROP_ORDER must cover all 10 SPEC §5 tables",
        );
        for t in TABLES_REVERSE_DROP_ORDER {
            assert!(
                GENESIS_DDL.contains(&format!("CREATE TABLE IF NOT EXISTS {t} (")),
                "down would drop `{t}` but up never creates it",
            );
        }
    }

    /// Every enum type the entities declare via `DeriveActiveEnum` (`ENTITIES-12`)
    /// is created as a real pg `ENUM` and referenced by at least one column.
    #[test]
    fn every_enum_type_is_created_and_used() {
        // 13 pg-enum types across the schema (one per DeriveActiveEnum).
        assert_eq!(ENUM_TYPES.len(), 13, "expected 13 pg-enum types");
        for (name, _) in ENUM_TYPES {
            assert!(
                GENESIS_DDL.contains(name),
                "enum type `{name}` is created but no column uses it",
            );
        }
    }

    /// `BUDGET-MONEY-1` / `DOMAIN-8`: no money column may be a float type. The
    /// DDL must never contain `REAL`, `DOUBLE`, `FLOAT`, or `MONEY`; money is
    /// `NUMERIC(18,2)`.
    #[test]
    fn no_floating_point_money_columns() {
        for banned in ["REAL", "DOUBLE", "FLOAT8", "FLOAT4", " MONEY"] {
            assert!(
                !GENESIS_DDL.contains(banned),
                "DDL contains banned float-ish money type `{banned}` (BUDGET-MONEY-1)",
            );
        }
        assert!(
            GENESIS_DDL.contains("NUMERIC(18, 2)"),
            "expected NUMERIC money columns",
        );
    }

    /// `SQL-DB-INDEX-1`: every FK column has an index declared in this migration.
    /// Checks the explicit FK-index names for each foreign key in the schema.
    #[test]
    fn every_fk_column_has_an_index() {
        let fk_indexes = [
            "ix_budgets_user_id",
            "ux_paycheck_config_user_id", // unique (one row per user), doubles as the FK index
            "ix_funds_user_id",
            "ix_categories_budget_id",
            "ix_plaid_items_user_id",
            "ix_accounts_user_id",
            "ix_accounts_plaid_item_id",
            "ix_months_user_id",
            "ix_months_budget_id",
            "ix_transactions_user_id",
            "ix_transactions_month_id",
            "ix_transactions_category_id",
            "ix_transactions_account_id",
            "ix_repayment_obligations_user_id",
            "ix_repayment_obligations_fund_id",
            "ix_repayment_obligations_transaction_id",
        ];
        for ix in fk_indexes {
            assert!(
                GENESIS_DDL.contains(ix),
                "missing FK index `{ix}` (SQL-DB-INDEX-1)",
            );
        }
    }

    /// The four SPEC §12 DB constraints are all present.
    #[test]
    fn spec_twelve_constraints_present() {
        // 1. One rollover bucket per budget version (partial unique).
        assert!(
            GENESIS_DDL.contains(
                "CREATE UNIQUE INDEX IF NOT EXISTS ux_categories_one_rollover_per_budget"
            )
        );
        assert!(GENESIS_DDL.contains("ON categories (budget_id) WHERE is_rollover_bucket"));

        // 2. At most one rollover transaction per month (partial unique).
        assert!(
            GENESIS_DDL.contains(
                "CREATE UNIQUE INDEX IF NOT EXISTS ux_transactions_one_rollover_per_month"
            )
        );
        assert!(GENESIS_DDL.contains("ON transactions (month_id) WHERE is_rollover"));

        // 3. plaid_transaction_id UNIQUE (partial WHERE NOT NULL — Plaid dedup).
        assert!(
            GENESIS_DDL
                .contains("CREATE UNIQUE INDEX IF NOT EXISTS ux_transactions_plaid_transaction_id")
        );
        assert!(GENESIS_DDL.contains("WHERE plaid_transaction_id IS NOT NULL"));

        // 4. UNIQUE(user_id, year, month) on months (idempotent lazy-init).
        assert!(
            GENESIS_DDL.contains("CREATE UNIQUE INDEX IF NOT EXISTS ux_months_user_year_month")
        );
        assert!(GENESIS_DDL.contains("ON months (user_id, year, month)"));
    }

    /// `category_key` (D3) and `tracking_start_date` (D8) columns exist.
    #[test]
    fn lineage_and_cutover_columns_present() {
        assert!(GENESIS_DDL.contains("category_key       UUID         NOT NULL"));
        assert!(GENESIS_DDL.contains("tracking_start_date DATE        NOT NULL"));
    }

    /// `PROC-CI-MIGRATION-HYGIENE-1`: every `CREATE TABLE` / `CREATE INDEX` is
    /// `IF NOT EXISTS` for re-run safety; no bare creates slip through.
    #[test]
    fn all_creates_are_idempotent() {
        for line in GENESIS_DDL.lines() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("CREATE TABLE") {
                assert!(
                    trimmed.starts_with("CREATE TABLE IF NOT EXISTS"),
                    "non-idempotent CREATE TABLE: `{trimmed}`",
                );
            }
            if trimmed.starts_with("CREATE INDEX") || trimmed.starts_with("CREATE UNIQUE INDEX") {
                assert!(
                    trimmed.contains("IF NOT EXISTS"),
                    "non-idempotent CREATE INDEX: `{trimmed}`",
                );
            }
        }
    }
}
