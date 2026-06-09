//! m0002 — auth schema (`BUDGET-AUTH-GATE-1`, `SPEC §5` / `§9.1`).
//!
//! Adds the `webauthn_credentials` table (passkeys / biometric login: one user,
//! many devices). The genesis migration (m0001) deliberately did not include it
//! because the auth slice (build step 7) is where passkeys land; this is a new
//! migration appended to the runner, never an edit to a shipped one
//! (`ARCH-EXPAND-CONTRACT-1` / `PROC-CI-MIGRATION-HYGIENE-1`).
//!
//! ## What is NOT here: the session store table
//!
//! Sessions are Postgres-backed via a **server-side session store that manages
//! its own table** (`SPEC §5` / `§9.1`: "the store manages its own table").
//! `tower-sessions`' Postgres store creates and migrates its own
//! `tower_sessions.session` table on startup (`SessionStore::migrate`), so the
//! session table is intentionally NOT defined in this app migration — owning it
//! here would duplicate (and risk drifting from) the store's own schema.
//!
//! ## Why raw SQL
//!
//! Same rationale as m0001: `BYTEA` columns, a partial / single-column unique
//! index, and explicit `IF NOT EXISTS` idempotency are clearest as raw DDL
//! (`PROC-CI-MIGRATION-HYGIENE-1`). `RUST-SEAORM-RAW-SQL-ESCAPE-1` governs
//! repository reads, not migrations.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(WEBAUTHN_DDL)
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared("DROP TABLE IF EXISTS webauthn_credentials CASCADE;")
            .await?;
        Ok(())
    }
}

/// The `webauthn_credentials` DDL (`SPEC §5`). FK index + the `credential_id`
/// unique index are declared in the same migration as the table
/// (`SQL-DB-INDEX-1` / `SQL-DB-INDEX-2`).
const WEBAUTHN_DDL: &str = r"
-- ===================== webauthn_credentials ===========================
-- Passkeys / biometric login (SPEC §5, §9.1; BUDGET-AUTH-GATE-1). One user,
-- many devices. credential_id + public_key are opaque authenticator-chosen
-- byte strings (BYTEA). sign_count is the authenticator signature counter
-- (clone detection), bumped on each successful assertion.
CREATE TABLE IF NOT EXISTS webauthn_credentials (
    id            UUID        PRIMARY KEY,
    user_id       UUID        NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    credential_id BYTEA       NOT NULL,
    public_key    BYTEA       NOT NULL,
    sign_count    BIGINT      NOT NULL DEFAULT 0,
    transports    TEXT,
    aaguid        TEXT,
    nickname      TEXT,
    created_at    TIMESTAMPTZ NOT NULL,
    last_used_at  TIMESTAMPTZ
);
-- DB-INDEX-1: FK column index.
CREATE INDEX IF NOT EXISTS ix_webauthn_credentials_user_id
    ON webauthn_credentials (user_id);
-- SPEC §5: credential_id UNIQUE (the assertion path looks a credential up by it,
-- so this doubles as the DB-INDEX-2 lookup index).
CREATE UNIQUE INDEX IF NOT EXISTS ux_webauthn_credentials_credential_id
    ON webauthn_credentials (credential_id);
";

#[cfg(test)]
mod tests {
    //! DB-free structural assertions over the m0002 DDL (`ORCH-NEW-PATH-TESTS-1`).
    //! The live apply/down round-trip is exercised by the `DATABASE_URL`-gated
    //! migration integration test.

    use super::WEBAUTHN_DDL;

    #[test]
    fn ddl_creates_webauthn_table() {
        assert!(
            WEBAUTHN_DDL.contains("CREATE TABLE IF NOT EXISTS webauthn_credentials ("),
            "missing CREATE TABLE for webauthn_credentials",
        );
    }

    #[test]
    fn fk_and_unique_indexes_present() {
        // SQL-DB-INDEX-1: FK column index.
        assert!(WEBAUTHN_DDL.contains("ix_webauthn_credentials_user_id"));
        // SPEC §5: credential_id UNIQUE.
        assert!(WEBAUTHN_DDL.contains("ux_webauthn_credentials_credential_id"));
        assert!(WEBAUTHN_DDL.contains("ON webauthn_credentials (credential_id)"));
    }

    #[test]
    fn opaque_blobs_are_bytea_not_text() {
        assert!(WEBAUTHN_DDL.contains("credential_id BYTEA       NOT NULL"));
        assert!(WEBAUTHN_DDL.contains("public_key    BYTEA       NOT NULL"));
    }

    #[test]
    fn all_creates_are_idempotent() {
        for line in WEBAUTHN_DDL.lines() {
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

    #[test]
    fn fk_has_cascade_delete() {
        assert!(
            WEBAUTHN_DDL.contains("REFERENCES users (id) ON DELETE CASCADE"),
            "webauthn_credentials.user_id must cascade-delete with the user",
        );
    }
}
