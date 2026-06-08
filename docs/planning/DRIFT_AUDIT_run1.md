# Drift Audit — Backend Foundation (Run #1)

Scope: the committed backend **foundation only** (PHASE_1_REPORT §8 steps 1–2):
Cargo workspace, SeaORM entities for the SPEC §5 schema, the domain layer
(`Money`, newtype IDs, validated strings, enums, error types, aggregate structs,
repository **traits**, `UowProvider` trait), the entity↔domain mappers, and the
Terraform/CI skeleton. Steps 3–9 (repository **implementations**, services,
month/rollover lifecycle, funds, income, auth, Plaid client, seeding) are **not
built yet**; their absence is `not_built`, not drift. All `webauthn_credentials`
/ auth / PWA code is a post-foundation slice and is likewise `not_built`.

Method: every non-trivial finding from the four review dimensions was
adversarially re-read against the cited `file:line`. Confirmed findings are kept;
unconfirmable ones are dropped and noted. Overlapping findings are deduplicated.

---

## Executive verdict

**MINOR DRIFT.** The committed foundation is, with one exception, clean and
spec-aligned: all 10 SPEC §5 tables are present as SeaORM entities with correct
types, `Money` is exact-decimal and float-free, the domain layer is complete and
hexagonally pure, the two named budget predicates
(`BUDGET-STATUS-DRIVES-INCLUSION-1`, `BUDGET-NO-DOUBLE-CHARGE-1`) are correct and
meaningfully tested, and all four quality gates pass.

The **one confirmed drift** is a false safety net: the Plaid sign-direction
`debug_assert!` in `plaid_model_to_domain` is **tautological** — it compares the
flipped value against itself and can never fire, despite a doc comment promising
it guards against a Plaid API sign-convention change (`BUDGET-PLAID-SIGN-1`). The
sign-flip arithmetic itself is correct; only the protective claim is illusory.
This is built-but-wrong (a guard that does not guard), hence `drift`, not
`not_built`.

Everything else classified by the source dimensions as "drift/high" reduced on
re-read to `not_built` (the constraint layer lives in a migration that is a later
step) or to `coverage`/`note`. There is **no schema, domain, or mapper drift**.

---

## Quality gate re-verification

All four gates re-verified PASS. All five crates inherit `[lints] workspace =
true`; the workspace declares `unsafe_code = "forbid"`, `unwrap_used = "deny"`,
`expect_used = "deny"`, `todo = "warn"`, `unimplemented = "warn"`.

| Gate | Command | Result |
|------|---------|--------|
| 1. Format | `cargo fmt --all -- --check` | **PASS** (exit 0, no output) |
| 2. Lint | `cargo clippy` (CI: `-W clippy::pedantic -D warnings -D clippy::unwrap_used -D clippy::expect_used`) | **PASS** (no warnings/errors, all 5 crates) |
| 3. Test | `cargo test --workspace` | **PASS** (62 passed, 0 failed, 1 deliberate `ignore` doctest in `uow.rs`) |
| 4. `unsafe` | `unsafe_code = "forbid"` + grep | **PASS** (declared workspace-wide, inherited by all 5 crates, zero `unsafe` in source) |

Per-crate tests: budget-domain 20 · budget-mappers 42 · budget-entities 0
(data-shape only) · budget-infrastructure 0 (stub) · budget-app-services 0 (stub).

`gates_green: true`.

---

## Confirmed DRIFT findings

| # | Sev | Rule / Spec | File:line | Problem | Fix |
|---|-----|-------------|-----------|---------|-----|
| D1 | **medium** | `BUDGET-PLAID-SIGN-1` | `crates/budget-mappers/src/transactions.rs:155,163-168` | The direction guard is tautological. `internal_amount = Money::from_decimal(-plaid_raw)`, then `debug_assert!(plaid_raw.is_zero() || internal_amount.as_decimal().is_sign_negative() == plaid_raw.is_sign_positive())`. Since `internal` is **exactly** `-plaid_raw`, for every nonzero input `(-plaid_raw).is_sign_negative()` is true iff `plaid_raw > 0`, and `plaid_raw.is_sign_positive()` is also true iff `plaid_raw > 0` — both sides are equal **by construction of negation**. The assert holds for all inputs and can never fire. The doc comment (lines 151-162) claims it is a "runtime direction test" that "fires if Plaid's inflow sign ever changes" and that tests "panic … for catching `BUDGET-PLAID-SIGN-1` regressions," but it asserts a property of negation, not a property of Plaid's convention. The three sign tests (`plaid_positive_becomes_negative_expense`, `plaid_negative_becomes_positive_inflow`, `zero_amount_passes_direction_test`, lines 236-264) only verify the negation arithmetic, not the assert's protective claim. **The sign flip itself is correct; only the safety net is illusory.** | Either (a) replace the assert with a real direction check keyed off an independent signal (e.g. a known-direction fixture, or Plaid's `amount` vs the transaction `category`/type), or (b) **downgrade the doc comment** to state it merely documents the flip, and drop the false "fires if Plaid changes its convention" / "catching regressions" language. Lowest-effort honest fix is (b); the durable fix is (a) plus a fixture-backed test that would actually break on a sign flip. |

No other drift confirmed. Findings the source dimensions tagged drift/high that
were **re-read and reclassified**: the §12 constraint layer (high → `not_built`,
see N1 below — the constraints are deferred to a migration that is a later step,
not built-but-wrong).

---

## Not-built (expected-absent, for context)

These are correctly absent at the foundation stage; listed so the next run has a
complete picture. None are drift.

- **N1 — §12 DB constraints not yet materialized** (`SPEC §12 D#11`;
  `RUST-ENTITIES-7/8`; `BUDGET-ROLLOVER-INTEGRITY-1`;
  `BUDGET-IDEMPOTENT-MONTH-INIT-1`). The four constraints — partial unique on
  `categories(budget_id) WHERE is_rollover_bucket`, partial unique on
  `transactions(month_id) WHERE is_rollover`, UNIQUE on
  `transactions.plaid_transaction_id`, and `UNIQUE(user_id, year, month)` on
  `months` — exist **only as prose** in entity module docs
  (`categories.rs:13-15,78-80`; `transactions.rs:20-23,87-95`; `months.rs:11-15`).
  Verified: there is **no migration crate, no `.sql` file, no
  `sea-orm-migration`/`Migrator` dependency** anywhere (`infra/` is Terraform
  only: `main.tf`, `outputs.tf`, `variables.tf`, `versions.tf`). Per
  `RUST-ENTITIES-7/8` these constraints "are enforced exclusively in the
  migration." With no migration they are enforced nowhere **yet**. This is
  correctly deferred (the migration runner is an open design item per the
  migration-replication thread), so `not_built` — **but note:**
  `.build-progress.md` step 2 is titled "Schema … All tables from SPEC.md §5"
  and marked `[x]` done, which **overclaims**: the field-level schema is done,
  the constraint layer is not. Recommend the next run land the migration runner
  before any repo impl relies on these guards at runtime (lazy-init idempotency
  and Plaid dedup both depend on them).
- **N2 — repository implementations** (`ARCH-REPO-PER-AGGREGATE-1`).
  `budget-infrastructure/src/lib.rs` is an 8-line doc-comment stub. No
  `SeaOrm*Repository`, no `SeaOrmUow`/`SeaOrmUowProvider`, no Key Vault / Plaid
  client. Step 3+.
- **N3 — app services** (`ARCH-STRICT-LAYERING-1`).
  `budget-app-services/src/lib.rs` is a 10-line stub. Month lifecycle, rollover
  posting, funds, income, Plaid sync = steps 3–8.
- **N4 — net-leftover (D5) and fund-earmark exclusion (D6) formulas** (`SPEC §12
  D5/D6`; `BUDGET-ROLLOVER-INTEGRITY-1`; `BUDGET-FUND-EARMARK-1`). Correctly
  **not** expressed as domain predicates; they belong to `MonthLifecycleService`
  / `FundService` (step 3-4). The domain already supplies every input
  (`counts_in_budget`, `fixed_category_spent`,
  `PaycheckConfigRepository::find_for_user`,
  `TransactionRepository::list_for_month` + `is_income`,
  `BudgetRepository::list_categories`). This is where a double-count or
  income-term-omission bug would most easily appear — give them oracle-style
  tests when authored.
- **N5 — `From<sea_orm::DbErr>` → `RepositoryError` mapping** (`DOMAIN-4/6`).
  Documented (unique→`UniqueViolation`, fk→`ForeignKeyViolation`,
  serialization-failure→`TransactionConflict`, else→`Database`) but lives in
  `budget-infrastructure`, which is a stub. SQLSTATE classification correctness
  cannot be audited until the impl exists — verify in the services/infra run.
- **N6 — `webauthn_credentials` table / all auth / PWA code.** No entity, domain
  type, or mapper. Correct: SPEC §12 places it in the auth slice (step 7).

---

## Coverage

Tests are strong where they exist (`Money` has float-trap, decimal-oracle, and
rolling-balance-loses-zero-cents property tests; both budget predicates assert
the *invariant* — e.g. `assert_ne!(spent, placeholder + real_txn)` — not a
trivial round-trip). Confirmed coverage gaps:

- **C1 (medium) — untested domain branch logic** (`ORCH-NEW-PATH-TESTS-1`; `SPEC
  §4.7/§4.8`). Five methods with real branches have **no dedicated unit tests**:
  `Cadence::period_months` (1/3/6/12 per variant), `Cadence::is_sinking_fund`,
  `PaycheckType::paychecks_per_year` (Some(24/26/52) | None for Hourly),
  `Category::is_sinking_fund` (compound: `cadence.is_sinking_fund() ||
  period_months.is_some_and(|m| m > 1)`, `category.rs:57-59`),
  `Category::effective_period_months` (the `period_months` override branch,
  `category.rs:64-69`), `Category::accrual_per_month` (`category.rs:74-76`). The
  mapper test `sinking_fund_category_round_trips` exercises `accrual_per_month`
  for the Annual case **indirectly only**; the Quarterly/Semiannual cadence
  paths, the `period_months` override branch, and all `PaycheckType` variants are
  uncovered. Not blocking at foundation, but these feed step-3 income and
  sinking-fund accrual — test them before services consume them.
- **C2 (low) — thin delegating wrappers, acceptable.**
  `Transaction::counts_in_budget` (`transaction.rs:68`) delegates to the
  exhaustively-tested predicate; `Transaction::is_income` (`transaction.rs:74`)
  is `income_kind.is_some()`, exercised indirectly by
  `income_kind_maps_when_present`. No branches of their own — acceptable, noted
  for completeness.
- **C3 (low) — `budget-entities` has 0 tests** — correct; pure data-shape crate,
  no serde on `Model` (`ENTITIES-2`), covered by mapper round-trips.

---

## Notes (positive confirmations / minor hygiene)

- **All 10 SPEC §5 tables present** with every column and correct types; all
  NUMERIC columns map to `rust_decimal::Decimal` (`BUDGET-MONEY-1`/`DOMAIN-8`);
  all pg-enum columns use typed `DeriveActiveEnum` (`ENTITIES-12`). Zero
  `f32`/`f64` in source (only doc-comment mentions). Confirmed.
- **`Money` is exact and float-free** (`BUDGET-MONEY-1`/`DOMAIN-8`): single
  rounding point (`round_to_cents` = `round_dp(2)`, banker's rounding;
  `divide_into` guards `n==0`→`ZERO`, no panic, `money.rs:114-130`). Confirmed.
- **Both named predicates correct and single-source** (`predicates.rs:30-35`,
  `72-81`): `counts_in_budget` encodes Settled+Expected-in / Pending-out;
  `fixed_category_spent` returns placeholder XOR sum, never both;
  `Transaction::counts_in_budget` delegates rather than re-matching. Confirmed.
- **Domain layer complete and hexagonally pure** (`DOMAIN-1..8`, `REPO-1..10`):
  11 newtype IDs, `Email`+`AccessTokenRef` validated newtypes, three thiserror
  enums, one repo trait per aggregate (writes thread `Option<&dyn UnitOfWork>`,
  reads omit it), object-safe `UnitOfWork`+`UowProvider`. No
  SeaORM/tokio/axum/ORM dependency in `budget-domain`. Confirmed.
- **`category_key` (D3) and `tracking_start_date` (D8/`BUDGET-CUTOVER-1`)**
  present at entity/domain/mapper level as additive affordances; no
  cross-version reporting logic built (correctly deferred). `category_key` typed
  `Uuid` is a reasonable choice (SPEC §5 only says "stable lineage id").
  Confirmed.
- **Minor hygiene (low) — dead `serde` dep.**
  `crates/budget-entities/Cargo.toml:14` lists `serde = { workspace = true }`
  but no entity source imports/derives serde (`lib.rs:16` explicitly states "no
  serde on Model, `ENTITIES-2`"); `sea-orm` pulls serde transitively. **Not** an
  `ENTITIES-2` violation (that rule concerns `Model` derives, not Cargo.toml).
  Worth removing to avoid implying serde is intentionally available to entities.
- **No TODO/FIXME/`todo!()`/`unimplemented!()`** anywhere; placeholder crates use
  doc-comment prose only. Workspace clippy `todo`/`unimplemented = "warn"` would
  catch accidental stubs. Confirmed.
- **`.build-progress.md` is accurate** about steps 1-2 done / 3-9 pending, with
  the **one overclaim** noted in N1 (step 2's "all tables" title vs. the missing
  constraint layer).

---

## Recommended fixes for backend run #2

1. **(D1, medium) Fix the Plaid sign guard.** Replace the tautological
   `debug_assert!` in `transactions.rs:163-168` with either a real
   independent-signal direction check + a fixture-backed test that breaks on a
   sign flip, or at minimum rewrite the doc comment (lines 151-162) to stop
   claiming protection it does not provide. Honest doc is the floor; a real guard
   is the goal.
2. **(N1, high-priority-for-next-step) Land the migration runner** so the four
   §12 constraints (rollover partial-uniques, `plaid_transaction_id` UNIQUE,
   `UNIQUE(user_id,year,month)`) are materialized **before** any repo impl relies
   on them at runtime. Lazy-init idempotency
   (`BUDGET-IDEMPOTENT-MONTH-INIT-1`) and Plaid dedup (SPEC §6) depend on these
   DB guards. Correct the `.build-progress.md` step-2 title to reflect that the
   constraint layer ships with the migration, not with the entities.
3. **(C1, medium) Add direct unit tests** for `Cadence::period_months`,
   `Cadence::is_sinking_fund`, `PaycheckType::paychecks_per_year`,
   `Category::is_sinking_fund`, `Category::effective_period_months`,
   `Category::accrual_per_month` — cover the Quarterly/Semiannual paths, the
   `period_months` override branch, and every `PaycheckType` variant
   (`ORCH-NEW-PATH-TESTS-1`).
4. **(N4) When authoring `MonthLifecycleService`/`FundService`,** give the D5
   net-leftover formula and D6 fund-earmark exclusion oracle-style tests — these
   are the highest-integrity rules and the likeliest spot for a double-count.
5. **(N5) Verify the `From<DbErr>` SQLSTATE classification** when
   `budget-infrastructure` is implemented (unique/fk/serialization-failure
   mapping).
6. **(hygiene, low) Remove the dead `serde` dependency** from
   `crates/budget-entities/Cargo.toml`.
