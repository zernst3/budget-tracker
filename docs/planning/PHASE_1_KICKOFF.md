# Budget Tracker — Phase 1 Kickoff: Planning & Investigation Routine

**You are running Phase 1 of 2: a PLANNING & INVESTIGATION routine. Your ONLY deliverable is a written
REPORT for human review. Do NOT write application code, scaffolding, migrations, or Terraform in this
phase — Phase 2 (a separate routine) builds after this report is reviewed and approved.**

---

## Inputs (read FULLY before reporting)
1. **Requirements:** `New Projects/budget-tracker/SPEC.md` — the complete, authoritative spec. Read all
   of it. It is detailed and settled; treat its decisions as requirements, not suggestions.
2. **Rule sources:** Locate and read (a) the **Camerata** Rust rule library (the principle / CONVENTIONS
   set), and (b) the **Agora-rs** `CONVENTIONS.md` and its architectural patterns. These are the
   existing rule sets you will apply and cite.

## Locked technical stack (decided — do NOT re-litigate)
- **Backend:** Rust monolith, **Axum + SeaORM**, mirroring the Agora-rs port patterns — layered
  Controllers → Services → Repositories; intra-aggregate transactions via begin/commit; a UnitOfWork at
  the service layer for cross-aggregate work.
- **Frontend:** **Dioxus**, using **chorale** (Zach's table library) for the transactions UI.
- **chorale dependency (pre-release):** depend on it via a **git dependency on the `main` branch** of
  `https://github.com/zernst3/rust-chorale` (the branch holding the v0.2.0 work) until chorale v0.2.0 is
  published to crates.io, after which it is swapped to the crates.io version (`chorale-dioxus = "0.2.0"`).
  API churn on `main` is EXPECTED and acceptable — building against it is deliberate dogfooding.
- **Database:** **PostgreSQL on Neon** (free serverless tier — NOT Azure Postgres), provisioned via the
  **Neon Terraform provider**. The connection string flows into Key Vault / app config via Terraform.
- **Infrastructure — EVERYTHING under Terraform (one config, multiple providers):**
  - **Azure Container Apps** (scale-to-zero) — the monolith.
  - **Neon** — the Postgres database.
  - **Azure Key Vault** — the Plaid access token (as a secret reference) + DB credentials.
  - **GitHub Container Registry** — the container image (NOT Azure Container Registry).
- **CI/CD — GitHub Actions:** quality gates (clippy at pedantic + `unwrap_used`/`expect_used`, full test
  suite, `cargo fmt --check`, `unsafe` forbidden) → build image → push to GHCR → deploy to Azure
  Container Apps on merge to `main`. Match chorale/Agora's quality bar; tests are a hard merge gate.
- **Bank integration — Plaid, Transactions product ONLY (read-only):**
  - In-app **Plaid Link** widget for the BoA OAuth (the app never sees the bank password).
  - Backend exchanges the `public_token` for an `access_token`, stored as a **Key Vault secret
    reference — NEVER raw in the DB**.
  - **Do NOT enable the Transfer product** — the token must be physically incapable of moving money.
  - Incremental sync via **`/transactions/sync` (cursor-based)** PLUS the **rolling 30-day reconcile on
    every pull** (SPEC §6).
- **Auth:** single-user, real auth (TOTP), reusing Agora's Key Vault + TOTP patterns. Build **NO**
  multi-user features (schema is multi-user-shaped, code is not).

---

## Your task (each item = a section in the report)
1. **Confirm understanding of the requirements.** Restate the spec's model in your own words; **flag any
   ambiguities, gaps, or internal contradictions** you find — list them, do NOT silently resolve them.
2. **Map requirements → applicable Camerata / Agora rules.** For each major area — data model,
   repositories/UoW, the rolling-Other balance, flexible-set settlement, sinking funds, income modes +
   smoothing buffer, the buffer/surplus large-purchase model, lazy month-init + multi-month catch-up,
   Plaid sync + 30-day reconcile, initial-load seeding, auth/secrets — **cite the specific Camerata /
   Agora rule IDs** that apply.
3. **Suggest PROJECT-SPECIFIC rules** this domain needs that the existing libraries don't cover — at
   minimum: the no-double-charge invariant (§4.5) as an enforceable rule; rolling-balance integrity;
   "Plaid token never in the DB"; **money amounts must use a fixed-precision decimal / integer-cents
   type, NEVER a float**. Propose each in the Camerata format (ID, one-line statement, rationale,
   example, and a test for what qualifies).
4. **Propose the architecture:** the layered module structure, the aggregates and their repositories
   (budgets, categories, months, transactions, funds, plaid_items, accounts), the domain model, and
   where each tricky invariant lives.
5. **Deep-dive the hard logic** — for EACH, state your intended approach: the rolling Other bucket as a
   system-generated transaction; flexible_set pending→settled + no-double-charge; sinking funds (cadence
   accrual, reset-on-payment); income per-paycheck vs smoothed + the smoothing buffer; the
   buffer/surplus + `repayment_obligations` large-purchase model; lazy month-init handling a MULTI-month
   gap idempotently; the Plaid cursor sync + 30-day reconcile; the initial-load per-category summary
   seeding; **expected expenses** (§4.10 — manual placeholders that RESERVE budget in a target month,
   the OPPOSITE budget treatment from Plaid pending, settle-on-match via the no-double-charge pattern).
6. **Infra plan:** the Terraform layout (providers, resources, the secret flow from Neon → Key Vault →
   app), and the CI/CD pipeline stages.
7. **Risks, open questions, and one-way-door decisions** that need human sign-off before Phase 2.
8. **Propose the Phase-2 build plan & sequencing:** what to build, in what order, and — per the spec's
   **"design-complete, build-what-you-use"** discipline (§4.8) — **what to STUB**: build Zach's
   semimonthly-fixed income mode; STUB the bi-weekly/weekly/hourly modes and the income smoothing
   buffer; but DESIGN the schema for all of them so no future migration is needed.

---

## Output
A single structured **report** (markdown), one section per task item above, rule IDs cited throughout
for traceability. End with an explicit **"DECISIONS NEEDED BEFORE PHASE 2"** list. **Write no
application code, migrations, or Terraform** — this phase produces the plan, not the build.
