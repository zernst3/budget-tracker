# Budget Tracker — Phase 1 Report: Planning & Investigation

**Phase 1 of 2. This document is the plan, not the build. No application code, migrations, or Terraform are written in this phase.**

Inputs read in full: `SPEC.md` (authoritative requirements), the **Camerata** Rust rule library (`~/Documents/Repos/camerata-ai/principles/`, 105 rules), and the **Agora-rs** CONVENTIONS (`agora-mono/CONVENTIONS.md` + `agora-mono/agora-rs/docs/CONVENTIONS.md`, 59 rules).

### How rule citations work in this report
Camerata is the portable rule library; Agora-rs CONVENTIONS is the worked reference implementation of those same rules in a real Rust/Axum/SeaORM/Dioxus codebase. They are the same architecture at two levels. Throughout, I cite **`Camerata:ID` / `Agora:ID`** as a pair when a rule exists in both. For a greenfield project, Camerata is the primary source of truth (it is the library designed to be adopted); Agora-rs is the pattern to copy when in doubt about how a rule looks in practice.

---

## 1. Confirmed understanding of the requirements

### Model restated
A single-user, self-hosted personal budget app that replaces a 4-year Google Sheet. The core difference from off-the-shelf tools (the entire reason to build) is a **rolling "Other" balance**: each month's total net leftover (surplus or overspend across all categories) carries into the next month as an **auditable, system-generated transaction**, not a mutated number.

The data model is **versioned budgets → categories → months → transactions**, plus **funds** (buffer/surplus/sinking) and **repayment_obligations**. Every aggregate is `user_id`-shaped for future multi-user but **zero multi-user code is built** (`Spec §9`).

Key behavioral primitives:
- **Categories** are `fixed` (with `true_set` or `flexible_set` settlement) or `discretionary`; exactly one is the rollover bucket; `cadence > monthly` makes a category a **sinking fund**.
- **No-double-charge** (`§4.5`): a fixed category's spent is `settled ? sum(txns) : placeholder`, never both.
- **Pending vs settled vs expected** (`§4.4`, `§4.10`): Plaid `pending` is **excluded** from budget math; manual `expected` placeholders are **included** (they reserve budget). Opposite handling, same `status` column.
- **Funds** are one primitive in three kinds: `buffer` (compulsory repayment, lean target), `surplus` (pre-saved, no repayment), `sinking` (auto-accrues toward a recurring bill). All three are **virtual envelopes** that reduce free-to-spend.
- **Large purchases** resolve one of three ways: `pay_in_full`, `pay_through_surplus(fund)`, or `buffer_financed` (creates a `repayment_obligation` with compulsory monthly installments back into the buffer).
- **Lazy month-init** is the source of truth (scale-to-zero kills any midnight timer): on access, create any missing months in order, post each rollover, idempotently, across a multi-month gap.
- **Plaid** is Transactions-product-only (Transfer never enabled), cursor-synced via `/transactions/sync`, plus a rolling 30-day reconcile each pull; the access token is a Key Vault secret reference, never raw in the DB.
- **Income** (`§4.8`) has two modes (`per_paycheck` default, `smoothed` opt-in with a smoothing buffer); Zach is semimonthly so both collapse to identical for him. Build his mode; stub the rest; design the schema for all.
- **Initial load** seeds the current month with one summary opening charge per category plus a correct starting buffer/Other balance. No history backfill.

This all coheres. The model is internally consistent at the conceptual level. The gaps below are at the **interaction boundaries** between primitives, which is exactly where a settled-looking spec tends to hide its remaining decisions.

### Ambiguities, gaps, and internal contradictions (flagged, NOT resolved)

1. **Rollover math × unsettled flexible_set placeholders** (the spec flags this itself in `§10`). Month net leftover = `sum(remaining across categories)`, and a `pending` flexible_set category counts its placeholder as spent. If a category is still `pending` at month-close, does its placeholder count toward the leftover that rolls into Other, even though the real bill has not landed? If yes, and the bill settles next month at a different amount, the rollover was computed on a fiction. Needs an explicit rule for how unsettled placeholders participate in (or are excluded from) the close-of-month net.

2. **Fund accrual × the rolling-Other leftover (double-count risk).** Sinking-fund accrual and the buffer/smoothing buffer all "reduce free-to-spend" as virtual envelopes. But month net leftover is `sum(remaining across categories)`. If $50 is earmarked into a fund this month, that $50 is not free, yet if it is not also subtracted from the leftover it will look like surplus and roll into Other, double-counting it (once in the fund, once in Other). The spec never states that **fund contributions are an expense against the month** for leftover purposes. This is the single most important integrity question and is currently undefined.

3. **Where does income live in the rolling balance?** Budget math in `§5` is expense-vs-budget per category; income is not a category. But the rolling Other is "net leftover including whether he was over/under on the TOTAL budget." Is the monthly budget expense-only (income tracked on the side and never touching Other), or does an income shortfall/surplus net into the rolling balance? `budgeted` income "reconciles against an expectation and resolves at month-end," which sounds like it DOES feed the net, but the leftover formula has no income term. Contradiction to resolve.

4. **Money sign convention, especially Plaid's.** `transactions.amount` is "signed; negative = expense" (`§5`), income positive, rollover signed. But **Plaid's API uses positive for outflows** (debits). The sign-flip at the Plaid mapping boundary is a classic source of silent dollar-direction bugs and is not called out. A single documented sign convention plus an explicit Plaid normalization step is needed.

5. **Timezone that defines "the month."** Lazy-init creates "the current month" and dates rollovers "the 1st." Camerata/Agora say timestamps are UTC (`Camerata:ARCH-UTC-TIMESTAMPS-1`, `Camerata:UI-UTC-DATES-1`). But "current month" for budgeting is a **local-calendar** concept, and Zach travels US/Europe. Is month membership decided by UTC, by a fixed home timezone, or by device-local time? This affects which month a late-night-of-the-1st transaction lands in.

6. **Category identity across budget versions.** `categories.budget_id → budgets`, so each budget version owns its own category rows. The "same" Groceries in the NYC budget and the Polish budget are different rows. Rollover crosses months that may cross budget versions, and reporting "Groceries over time" needs a stable cross-version identity. Is there a `category_key`/lineage linking a category across versions, or is cross-version continuity simply not supported in V1?

7. **Forward-dated expected expense into a not-yet-existing month.** `§4.10` allows reserving a FUTURE month's budget (book in June, reserve July). But months are lazily created, so July's `months` row does not exist yet, and `transactions.month_id → months`. Either `month_id` must be nullable and resolved at init, or future months must be eagerly created when an expected expense targets them. Interacts directly with lazy-init (item from `§4.6`).

8. **Buffer-financed purchase: how the full-price transaction avoids blowing up its month.** The large purchase is "marked spent immediately" in full, but the cash is "fronted by the buffer." If the full-price transaction hits its category, that month overspends massively; the buffer is supposed to absorb it. The offsetting mechanics (a buffer-draw entry that cancels the category hit, then compulsory monthly repayment line items) are implied but not specified. Where the full price lands in the month math needs to be pinned.

9. **Partial flexible_set settlement at month-close.** `expected_bills = 2` (electric + gas) settles only when both land. If only one has landed when the month closes, what is the category's spent: the one real bill, the placeholder, or a mix? `§4.10` defines stale handling for expected expenses but not for partially-settled flexible_set categories.

10. **Plaid `removed` transactions undoing settlement.** Cursor sync returns `removed`. If a removed transaction had already been categorized and settled a fixed category (or matched an expected expense), removing it must un-settle that category and restore the placeholder. The reconcile path handles `modified` but the `removed`-after-settlement cascade is unspecified.

11. **"Exactly one rollover bucket" scope.** `is_rollover_bucket` is "exactly one true" — per budget version, or globally per user? Since categories are per-version, it is presumably per version, enforced by a DB partial unique index. Worth confirming so the constraint is correct.

12. **REST controllers vs Dioxus server functions (architecture-level, see §4/§7).** The kickoff says "layered Controllers → Services → Repositories" (Agora REST pattern), but the spec also says "reuse the portfolio's Dioxus deploy pattern" (Dioxus Fullstack uses **server functions**, not a REST controller tier, per `Camerata:RUST-DIOXUS-9`). These are two different boundary shapes. Flagged here; treated as a one-way-door decision in §7.

---

## 2. Requirements → applicable Camerata / Agora rules

| Requirement area | Camerata rules | Agora-rs rules |
|---|---|---|
| **Data model** (entities, FKs, pg enums, unique constraints) | `RUST-ENTITIES-1..13` (esp. `-4` FK both sides, `-12` pgEnum→typed enum, `-7` DB-level uniques), `RUST-MAPPER-1` | `ENTITIES-1..13`, `MAPPER-1` |
| **Domain types** (newtype IDs, validated strings, money, errors, async) | `RUST-DOMAIN-1..7`, `ARCH-EXACT-DECIMALS-1` (money), `RUST-DOMAIN-4`/`-6` (thiserror) | `DOMAIN-1..9` (esp. `DOMAIN-8` `numeric`→`rust_decimal::Decimal`, `DOMAIN-9` `ShortId`) |
| **Repositories / UoW** | `ARCH-REPO-PER-AGGREGATE-1`, `ARCH-REPO-RETURNS-DOMAIN-1`, `ARCH-STRICT-LAYERING-1`, `ARCH-EXPLICIT-TX-1`, `RUST-DOMAIN-7` (explicit UoW param), `RUST-SEAORM-INTRA-AGGREGATE-TX-1`, `RUST-SEAORM-PROJECTION-TYPES-1`, `RUST-SEAORM-RAW-SQL-ESCAPE-1` | `REPO-1..11`, `SERVICE-1`, `SERVICE-TX-1`, `SERVICE-DI-1`, `SERVICE-ERROR-1` |
| **Rolling Other balance** | `ARCH-EXACT-DECIMALS-1`, `SQL-DB-NPLUSONE-1` (aggregations), `RUST-SEAORM-PROJECTION-TYPES-1` (computed views) | `DOMAIN-8`, `DB-NPLUSONE-1`, `REPO-9` |
| **flexible_set settlement / no-double-charge** | *(new project rule, §3 below)*, `ARCH-EXACT-DECIMALS-1` | *(no equivalent — domain-specific)* |
| **Sinking funds** (accrual, reset-on-payment, virtual envelope) | `ARCH-EXACT-DECIMALS-1`, `SPIRIT-ROBUSTNESS-1`, *(new project rules §3)* | `DOMAIN-8` |
| **Income modes + smoothing buffer** | `SPIRIT-ROBUSTNESS-1` ("buy complexity only against a named threat" governs the design-complete/build-what-you-use discipline), `SPIRIT-FILE-SIZE-1` | `CC-1` (robustness over terseness) |
| **Buffer/surplus large-purchase model** | `ARCH-EXACT-DECIMALS-1`, `ARCH-EXPLICIT-TX-1` (cross-aggregate writes), *(new project rules §3)* | `SERVICE-TX-1`, `REPO-10` |
| **Lazy month-init + multi-month catch-up** | `ARCH-IDEMPOTENCY-KEYS-1` (idempotency principle), *(new project rule §3)* | *(domain-specific)*; backstop tokio task per `WORKERS-1` |
| **Plaid sync + 30-day reconcile** | `ARCH-IDEMPOTENCY-KEYS-1`, `ARCH-CURSOR-PAGINATION-1` (cursor model), `ARCH-BOUNDARY-VALIDATION-1`, `ARCH-PARALLEL-INDEPENDENT-1` | `DB-NPLUSONE-1`, `SERVICE-PARALLEL-1` |
| **Initial-load seeding** | `SPIRIT-ROBUSTNESS-1`, *(new project rule §3)* | — |
| **Auth / secrets** | `ARCH-SERVER-AUTHZ-1`, `PROC-PERMISSION-CONFIG-1`, `ARCH-IAC-1` (secrets in IaC), *(new `BUDGET-PLAID-TOKEN-VAULT-1` §3)* | `AUTH-1`, `AUTH-2` |
| **Infra (Terraform, multi-provider)** | `ARCH-IAC-1`, `ARCH-TRIGGER-ENV-1`, `ARCH-EXPAND-CONTRACT-1`, `PROC-CI-MIGRATION-HYGIENE-1` | `MONOLITH-1` |
| **Workers** (lazy-init backstop, Plaid pull, retries) | `ARCH-PARALLEL-INDEPENDENT-1` | `WORKERS-1`, `MONOLITH-1`, `EMAIL-1`, `QUEUE-1` |
| **CI/CD + quality gate** | `ORCH-ENV-GATED-QUALITY-1`, `ORCH-NEW-PATH-TESTS-1`, `PROC-CI-MIGRATION-HYGIENE-1`, `PROC-AUTO-MERGE-1`, `PROC-CITE-CONVENTION-ID-1` | `CC-2` |
| **UI (chorale transactions table)** | `RUST-DIOXUS-1..16` (esp. `-9` server functions, `-10` capability flags, `-14` primitives layer), `UI-QUERY-LIBRARY-1`, `UI-UTC-DATES-1`, `UI-IMAGE-COMPONENT-1` | `UI-PARALLEL-1`, `UI-CACHE-1` |
| **Orchestrated build governance** | `ORCH-CLEAR-WINNER-1`, `ORCH-ONE-WAY-DOOR-1`, `ORCH-AUTOCALLS-LEDGER-1`, `ORCH-BUDGET-MONITOR-1`, `ORCH-MODEL-TIERING-1`, `ORCH-TIERED-ESCALATION-1` | `ROUTE-1` |

---

## 3. Proposed PROJECT-SPECIFIC rules (Camerata format)

These cover invariants the existing libraries do not. Each follows the Camerata schema (`id`, `title`, `tag`, `domain`, `layer`, `enforcement`, `default`, `summary`, `why`, `alternatives`) plus the kickoff-requested **`qualifies`** field (the conformance test for "what counts as following this rule"). Proposed new domain: `budget`.

### BUDGET-MONEY-1 — Money is an exact integer-cents or Decimal type, never a float
```toml
id = "BUDGET-MONEY-1"
title = "All monetary values use a fixed-precision Money type, never f32/f64"
tag = "stack"
domain = "budget"
layer = "language"
enforcement = "mechanical"
default = true
summary = """
Every monetary amount in the domain, the entities, the DTOs, and the UI uses a single Money type \
backed by rust_decimal::Decimal (mapping Postgres NUMERIC) or an i64 minor-units newtype. Floating \
point (f32/f64) is forbidden for any value that represents money. Arithmetic on money goes through \
the Money type, which rounds explicitly at defined points.
"""
why = """
Float rounding silently corrupts a budget tracked to the dollar: 0.1 + 0.2 != 0.3 in IEEE-754, and \
the rolling-Other balance compounds every month, so a one-cent drift becomes a permanent ledger \
error. This is the highest-consequence, lowest-cost invariant in the project. Mirrors Agora DOMAIN-8 \
and Camerata ARCH-EXACT-DECIMALS-1; elevated to mechanical here because money is the substance of the \
app, not an incidental field.
"""
alternatives = [
  "Use i64 minor units (cents) everywhere and only convert to Decimal for display",
  "Allow f64 internally but round at every boundary (rejected: rounding discipline is unenforceable and one missed site corrupts the ledger)",
]
qualifies = "A clippy/CI check (e.g. a disallowed-types lint or a grep gate) fails the build if f32/f64 appears in any type whose name or column maps to money. A property test asserts that summing N transactions and rolling the balance forward M months loses zero cents versus a Decimal oracle."
```

### BUDGET-NO-DOUBLE-CHARGE-1 — A fixed category's spent is settled-or-placeholder, never both
```toml
id = "BUDGET-NO-DOUBLE-CHARGE-1"
title = "Fixed-category spent is computed by one predicate: settled ? sum(txns) : placeholder"
tag = "stack"
domain = "budget"
layer = "library"
enforcement = "structured"
default = true
summary = """
The spent amount for a fixed category is produced by exactly one function: if the category is settled, \
spent = sum of its settled transactions; if unsettled, spent = the budgeted placeholder. The two are \
never added. Assigning a real transaction to an unsettled fixed category settles it and REPLACES the \
placeholder rather than stacking on top. This same settle-on-match predicate also governs flexible_set \
settlement (§4.2) and expected-expense matching (§4.10).
"""
why = """
This is the §4.5 invariant and the root of correctness for fixed expenses. Implemented as a flag or \
duplicated across call sites it WILL drift, double-counting rent the first time someone assigns a \
pulled transaction. As a single shared predicate it is enforceable and reusable across the three \
settlement surfaces (fixed, flexible_set, expected) so they cannot diverge.
"""
alternatives = [
  "A boolean has_real_txn flag toggled per category (rejected: flags drift from the underlying truth and invite the both-counted bug)",
  "Subtract the placeholder when the first real txn lands (rejected: equivalent result, more moving parts, breaks on multi-bill flexible_set)",
]
qualifies = "A unit test: create a fixed category with a placeholder, assign a real transaction, assert spent equals the real transaction (not placeholder + transaction). A second test: an unsettled category's spent equals the placeholder exactly. The spent computation exists in exactly one function (verified by grep/review)."
```

### BUDGET-ROLLOVER-INTEGRITY-1 — The rolling balance is an auditable transaction chain, posted idempotently
```toml
id = "BUDGET-ROLLOVER-INTEGRITY-1"
title = "Rollover is a system transaction (is_rollover=true), posted exactly once per month, never a mutated number"
tag = "stack"
domain = "budget"
layer = "library"
enforcement = "structured"
default = true
summary = """
The carryover into a month is materialized as a single system-generated transaction with \
is_rollover=true, category=the rollover bucket, dated the 1st, amount = the prior month's net leftover. \
The rolling balance is always the sum of this auditable chain; it is never stored as a mutable scalar. \
Posting a month's rollover is idempotent: a (month, is_rollover) uniqueness guard makes re-running \
lazy-init a no-op. Multi-month catch-up posts each missed month's rollover in chronological order.
"""
why = """
The rolling Other bucket is the entire reason the app exists (§4.3); its integrity is non-negotiable. \
A mutated number is unauditable and irrecoverable if a calculation is ever wrong. A transaction chain \
is inspectable ('April rollover: +$212'), reconstructable, and naturally idempotent under a uniqueness \
constraint, which is exactly what lazy-init on a scale-to-zero container requires (§4.6).
"""
alternatives = [
  "Store a running balance column on months and mutate it (rejected: unauditable, not idempotent, corrupts permanently on a single bad write)",
  "Recompute the whole chain from genesis on every load (rejected: unbounded work as history grows; the transaction chain already is the recomputation, persisted)",
]
qualifies = "A test runs lazy-init across a simulated 3-month gap and asserts exactly 3 rollover transactions exist with correct sequential amounts; running it again posts zero additional rows. A DB partial unique index on (month_id) WHERE is_rollover enforces single-posting."
```

### BUDGET-PLAID-TOKEN-VAULT-1 — The Plaid access token lives only as a Key Vault secret reference
```toml
id = "BUDGET-PLAID-TOKEN-VAULT-1"
title = "Plaid access tokens are stored only as Key Vault secret references, never raw in the DB or logs"
tag = "stack"
domain = "budget"
layer = "framework"
enforcement = "mechanical"
default = true
summary = """
The Plaid access_token is exchanged server-side and written immediately to Azure Key Vault; the \
database stores only the secret reference (plaid_items.access_token_ref). The raw token never persists \
in a DB column, never appears in logs or telemetry, and is fetched from Key Vault at call time. The \
app requests only the Transactions (+Accounts) product; the Transfer product is never enabled.
"""
why = """
A bank access token is the highest-value secret in the system. Storing it raw turns any DB read, \
backup, or log leak into account-history exposure. The read-only product scoping (§6) makes the token \
physically incapable of moving money; the Vault-reference rule keeps even the read token out of the \
blast radius of a database compromise. Mirrors Agora's Key Vault secret-reference pattern.
"""
alternatives = [
  "Encrypt the token at rest in the DB with an app-held key (rejected: the app process holding both ciphertext and key collapses to plaintext on compromise; Key Vault separates custody)",
]
qualifies = "A CI grep/secret-scan gate fails if a Plaid token literal or an access_token column write is present. A test asserts plaid_items has no raw-token column and that the Plaid client reads from the Vault. A startup assertion confirms only Transactions/Accounts products are configured."
```

### BUDGET-STATUS-DRIVES-INCLUSION-1 — One predicate decides budget inclusion by status
```toml
id = "BUDGET-STATUS-DRIVES-INCLUSION-1"
title = "Budget math includes 'settled' and 'expected', excludes 'pending', via a single shared predicate"
tag = "stack"
domain = "budget"
layer = "library"
enforcement = "structured"
default = true
summary = """
Whether a transaction counts toward budget math is decided by one predicate keyed on status: \
settled = included, expected = included (it reserves budget, §4.10), pending = excluded (§4.4). Every \
aggregation (category spent, month net leftover, free-to-spend) routes through this predicate. The \
opposite treatment of Plaid 'pending' (excluded) and manual 'expected' (included) is encoded once.
"""
why = """
Pending-excluded and expected-included are deliberately opposite handlings of the same status column \
(§4.10). If the inclusion test is reimplemented per aggregation, one site will inevitably get the \
polarity wrong and either reserve budget for transient pending charges or fail to reserve it for known \
commitments. One predicate makes the polarity a single, testable decision.
"""
alternatives = [
  "Filter status inline at each query (rejected: N copies of the polarity, guaranteed drift)",
  "Two columns (counts_in_budget bool + status) (rejected: redundant state that can contradict the status)",
]
qualifies = "A test enumerates all three statuses and asserts inclusion=true/true/false for settled/expected/pending against the single predicate. Grep confirms aggregations call the predicate rather than inlining a status filter."
```

### BUDGET-FUND-EARMARK-1 — Earmarked fund balances reduce free-to-spend and are excluded from the rollover leftover
```toml
id = "BUDGET-FUND-EARMARK-1"
title = "Fund contributions are an expense against the month; earmarked money never also counts as rollover surplus"
tag = "stack"
domain = "budget"
layer = "library"
enforcement = "structured"
default = true
summary = """
Money moved into any fund (sinking accrual, surplus contribution, buffer repayment) is treated as an \
expense against that month's budget and reduces free-to-spend. It is therefore excluded from the month \
net-leftover that becomes the next rollover, so the same dollar is never counted both as a fund balance \
and as Other surplus. Fund draws (sinking payout, surplus draw, buffer financing) are fund-draws, not \
re-charged budget expenses (reuses BUDGET-NO-DOUBLE-CHARGE-1).
"""
why = """
This resolves the most dangerous interaction gap in the spec (ambiguity #2): funds and the rolling \
balance both claim 'free-to-spend' semantics, and without an explicit single-counting rule an earmarked \
dollar inflates Other and gets spent twice. Making fund contribution an explicit month expense gives \
the virtual-envelope discipline (§4.7/§4.9) and keeps the rollover honest.
"""
alternatives = [
  "Track funds entirely outside the budget and let free-to-spend ignore them (rejected: removes the discipline that is the whole point of a virtual envelope)",
  "Net funds into the rollover instead of expensing them monthly (rejected: makes the rollover a mixed signal and hides fund health)",
]
qualifies = "A test: accrue $50 into a sinking fund in a month with otherwise $0 net; assert month net leftover is -$50 (not +$50 or $0) and the fund balance is +$50, so total system money is conserved. A conservation property test asserts sum(category remaining) + sum(fund balances) is invariant under a fund contribution."
```

### BUDGET-IDEMPOTENT-MONTH-INIT-1 — Lazy month-init is idempotent and gap-complete
```toml
id = "BUDGET-IDEMPOTENT-MONTH-INIT-1"
title = "Lazy month-init creates every missing month in order and never double-posts on re-entry"
tag = "stack"
domain = "budget"
layer = "library"
enforcement = "structured"
default = true
summary = """
On access, the app initializes months lazily: it finds the latest existing month, then creates each \
missing month up to the current one in chronological order, posting each month's rollover (per \
BUDGET-ROLLOVER-INTEGRITY-1) and resolving the correct budget version per month. The whole operation \
is idempotent and concurrency-safe: re-entry (including two requests racing on container wake) creates \
no duplicate months or rollovers. A midnight timer is at most an opportunistic backstop, never the \
source of truth.
"""
why = """
On a scale-to-zero container the timer cannot be relied on (§4.6); lazy-on-access is the only correct \
mechanism, and correctness there means surviving multi-month gaps and concurrent wake-up requests \
without double-posting. The UNIQUE(user_id, year, month) constraint plus the rollover uniqueness guard \
make this enforceable rather than hopeful.
"""
alternatives = [
  "Rely on a scheduled timer to roll months (rejected by §4.6: the container is asleep when it should fire)",
  "Initialize only the current month, skipping gaps (rejected: a 2-month absence would lose a rollover link and break the chain)",
]
qualifies = "Tests: (a) cold app with last month = March, current = June → three months created with linked rollovers; (b) calling init twice yields identical state; (c) two concurrent init calls (simulated) produce one set of rows (relies on UNIQUE + ON CONFLICT)."
```

> Additional candidates worth adopting but lower-priority: **BUDGET-PLAID-SIGN-1** (normalize Plaid's positive-outflow convention to the internal signed convention at the mapper boundary, with a test on direction); **BUDGET-SETTLE-ON-MATCH-1** (flexible_set and expected-expense settlement share one match-and-replace function). Both are really corollaries of rules above and can be folded in or split out in Phase 2.

---

## 4. Proposed architecture

### Workspace / crate layout (mirrors Agora-rs, `Camerata:RUST-DOMAIN-1`/`Agora:DOMAIN-1`, `Agora:ENTITIES-13`, `Agora:MAPPER-1`)
```
budget-tracker/
├── crates/
│   ├── budget-domain/         # domain types, newtype IDs, validated strings, repo traits,
│   │                          #   Money type, error enums (thiserror), UowProvider trait
│   ├── budget-entities/       # SeaORM entities (DeriveEntityModel), one file per table
│   ├── budget-mappers/        # entity <-> domain mapping (own crate, MAPPER-1)
│   ├── budget-infrastructure/ # repo impls, SeaOrmUow + SeaOrmUowProvider, Plaid client,
│   │                          #   Key Vault client, the in-process scheduler
│   ├── budget-app-services/   # services (business logic), AppServiceError
│   └── budget-app/            # Dioxus Fullstack crate: Axum host + server functions +
│                              #   SSR + the chorale transactions UI (single binary, MONOLITH-1)
├── infra/                     # Terraform (azurerm + neon + github providers)
├── .github/workflows/         # CI/CD
└── Cargo.toml                 # workspace
```
Rationale: this is the Agora layering (`Agora:REPO-1..11`, `SERVICE-1`, `SERVICE-DI-1`) with the UI folded into a single Dioxus Fullstack binary per `Camerata:RUST-DIOXUS-11`/`-16` and `Agora:MONOLITH-1`. Whether `budget-app` exposes services through **REST controllers** (Agora style) or **Dioxus server functions** (`Camerata:RUST-DIOXUS-9`) is the open decision in §7; the crate boundary is the same either way, only the thin top layer differs.

### Aggregates and their repositories (`Camerata:ARCH-REPO-PER-AGGREGATE-1`/`Agora:REPO-1`)
One trait per aggregate, declared in `budget-domain`, implemented in `budget-infrastructure`, returning domain types (`Agora:REPO-2`):
- **BudgetRepository** (budgets + versioned categories; category is part of the budget aggregate)
- **MonthRepository** (months; the lazy-init + rollover home)
- **TransactionRepository** (transactions, incl. system rollover rows, Plaid dedup by `plaid_transaction_id`)
- **FundRepository** (funds + repayment_obligations; repayment_obligation is part of the fund aggregate)
- **PlaidItemRepository** (plaid_items + accounts; account is part of the plaid_item/institution aggregate)
- **UserRepository** (users, auth, paycheck_config)

### Where each tricky invariant lives
- **Money type + sign convention:** `budget-domain` (`BUDGET-MONEY-1`, `Agora:DOMAIN-8`). Plaid sign normalization in `budget-mappers` (`BUDGET-PLAID-SIGN-1`).
- **No-double-charge / status-inclusion / settle-on-match predicates:** `budget-domain` (pure functions over domain types), called by services. These are domain rules, not data access (`BUDGET-NO-DOUBLE-CHARGE-1`, `BUDGET-STATUS-DRIVES-INCLUSION-1`).
- **Rolling-Other computation + lazy-init + multi-month catch-up:** a `MonthLifecycleService` in `budget-app-services`, cross-aggregate (months + transactions), so it owns a UnitOfWork (`Camerata:RUST-DOMAIN-7`/`Agora:SERVICE-TX-1`/`REPO-10`). Idempotency enforced by DB constraints (`BUDGET-IDEMPOTENT-MONTH-INIT-1`).
- **Fund accrual / draw / earmark accounting:** a `FundService` (cross-aggregate: funds + transactions + months), UnitOfWork-wrapped (`BUDGET-FUND-EARMARK-1`).
- **Large-purchase resolution (pay_in_full | pay_through_surplus | buffer_financed):** `FundService` + `TransactionService`, one atomic UoW per resolution (creates the txn, the fund draw, and any repayment_obligation together).
- **Plaid sync + 30-day reconcile:** a `PlaidSyncService` (infrastructure Plaid client + TransactionRepository), cursor stored on plaid_items; reconcile is a bounded 30-day re-fetch + diff.
- **Computed views (category spent, remaining, month net):** repository projection types (`Camerata:RUST-SEAORM-PROJECTION-TYPES-1`/`Agora:REPO-9`), aggregated in SQL to avoid N+1 (`Camerata:SQL-DB-NPLUSONE-1`/`Agora:DB-NPLUSONE-1`), never materialized.

---

## 5. Deep-dive: the hard logic (intended approach per item)

**Rolling Other as a system transaction.** At month-close (or lazy catch-up), compute `net = sum(category remaining)` for the closing month, where `remaining = budget_amount - spent` and `spent` uses the status-inclusion + no-double-charge predicates. Post one `transactions` row (`is_rollover=true`, category=rollover bucket, date=1st of the new month, amount=net). The Other bucket's live balance is the sum of its rollover chain plus any transactions categorized to it. Open ambiguities #1 (#unsettled placeholders in net) and #3 (income in net) must be resolved before this is implemented.

**flexible_set pending→settled + no-double-charge.** Category carries `settle_type=flexible_set`, `expected_bills=N`, and a derived state `pending|settled`. Spent = placeholder while `pending`; once `>=N` real transactions are assigned (or manual "mark settled"), state→`settled` and spent = sum(real txns), replacing the placeholder (`BUDGET-NO-DOUBLE-CHARGE-1`). Partial-settlement-at-close (ambiguity #9) needs a decision.

**Sinking funds (cadence accrual, reset-on-payment).** A category with `cadence > monthly` (or `period_months`) is a sinking fund with `fund_balance` + `next_due_date`. Monthly the lifecycle service accrues `amount / period_months` into `fund_balance`, recorded as a month expense (`BUDGET-FUND-EARMARK-1`) so it reduces free-to-spend and stays out of the rollover. When the real bill lands, the user tags it as the payout: draw `fund_balance` down, and reset the accrual clock forward from that date toward the next occurrence (forward-looking accrual). Mid-cycle setup seeds `next_due_date` + `current_balance` so the monthly accrual is computed correctly when started partway through.

**Income: per_paycheck vs smoothed + smoothing buffer.** `paycheck_config` holds `income_mode`, `paycheck_type`, `amount`, `anchor_date`, `surplus_routing`, `smoothing_buffer`. `per_paycheck` (default, built): expected = `paychecks_landing_this_month * amount`, derived from cadence + anchor; `amount` null → pure actual-tracking. `smoothed` (stubbed): expected = `amount * paychecks_per_year / 12`, constant, with the smoothing buffer absorbing 3-paycheck surpluses and funding 2-paycheck months. Surplus routing (both modes) applies the `surplus_routing` default with a per-transaction override. For Zach (semimonthly, exactly 2/month) the modes are identical and the buffer never fires; build his path, stub the rest, design the schema for all (§4.8 discipline). Ambiguity #3 (income's place in the rollover) blocks the exact expected-vs-actual reconciliation.

**Buffer/surplus + repayment_obligations (large purchases).** Resolution is chosen at purchase time: `pay_in_full` (normal expense), `pay_through_surplus(fund_id)` (draw the surplus fund, no repayment, fund-draw not re-charge), or `buffer_financed` (mark the purchase spent in full, draw the buffer to front the cash, create a `repayment_obligation` with `installment_amount` × `months_remaining`). Each month the lifecycle service posts the compulsory installment as a month expense flowing back into the buffer until `remaining_amount = 0` → `status=paid`. The exact bookkeeping that stops the full-price txn from blowing up its month (ambiguity #8) needs sign-off. Buffer health flags (above target → invest; below target with obligations → caution) are advisory only, never blocking (§4.9).

**Lazy month-init across a multi-month gap, idempotently.** On app load, find the latest month; for each missing month up to current, create it referencing the correct budget version for that month, post its rollover, in order. Idempotent via `UNIQUE(user_id, year, month)` + the rollover uniqueness guard (`BUDGET-IDEMPOTENT-MONTH-INIT-1`). Concurrency on container wake handled by `ON CONFLICT DO NOTHING` + transaction. Optional tokio backstop only when the container is already awake (`Agora:WORKERS-1`).

**Plaid cursor sync + 30-day reconcile.** Per plaid_item, call `/transactions/sync` with the stored `sync_cursor`; apply `added` (land uncategorized), `modified` (update, incl. pending→settled via `pending_transaction_id`), `removed` (delete, with the un-settle cascade of ambiguity #10). Then re-fetch the trailing 30 days and diff against stored rows to catch drift the cursor missed. Dedup by `plaid_transaction_id UNIQUE`. Sign normalized at the mapper (`BUDGET-PLAID-SIGN-1`). Token fetched from Key Vault (`BUDGET-PLAID-TOKEN-VAULT-1`).

**Initial-load seeding.** No history import. On first setup, for each category write one summary opening transaction = spend-so-far this month, and seed the starting buffer/Other balance from the reference xlsx. Track normally thereafter; real per-transaction rows accumulate on top of the opening summary lines.

**Expected expenses (§4.10).** A `transactions` row with `status='expected'`, `source='manual'`, a `category_id`, an amount, and a target month (which may be future). It is **included** in budget math (reserves budget) — the polarity opposite of Plaid `pending` (`BUDGET-STATUS-DRIVES-INCLUSION-1`). When the real charge arrives, the user matches it; the placeholder is replaced/settled by the real transaction (`BUDGET-NO-DOUBLE-CHARGE-1` / settle-on-match), never both. Forward-dating into a not-yet-created month is ambiguity #7. Unmatched at target-month close → prompt to carry forward or release (never silently drop).

---

## 6. Infra plan

### Terraform layout (`Camerata:ARCH-IAC-1`), one config, three providers
- **Providers:** `azurerm`, `neon`, `github` (or `integrations/github`).
- **Neon resources:** project, branch, database, role → outputs the connection string (sensitive).
- **Azure resources:** resource group; Container Apps environment + Container App (scale-to-zero, external ingress, user-assigned managed identity); Key Vault + role assignments (the identity gets Secrets User); Log Analytics workspace (kept under the 5 GB/mo free tier with non-verbose logging).
- **GitHub resources:** GHCR is the image registry (no ACR); the `github` provider manages the Actions variables/secrets and (optionally) the package. The container image is pulled from GHCR by the Container App via a registry credential/secret.
- **Secret flow:** Terraform takes the Neon connection string output → writes it as a **Key Vault secret** → the Container App references it as a **secret reference** (managed identity), surfacing it as an env var. The Plaid access token is **not** Terraform-managed (it is minted at runtime via Plaid Link); Terraform only provisions the Key Vault and the access policy, and the app writes/reads the token secret at runtime (`BUDGET-PLAID-TOKEN-VAULT-1`). DB credentials and any Plaid client_id/secret follow the same Vault-reference path.
- **State:** remote state (Azure Storage backend) so the multi-provider config has one source of truth.

### CI/CD pipeline (GitHub Actions, `Camerata:ORCH-ENV-GATED-QUALITY-1`, `PROC-CI-MIGRATION-HYGIENE-1`, `Agora:CC-2`)
Stages, on PR and on merge to `main`:
1. **Quality gate (hard):** `cargo fmt --check`; `cargo clippy --all-targets --all-features -- -D warnings` at pedantic plus `-D clippy::unwrap_used -D clippy::expect_used`; `unsafe` forbidden (workspace lint); full `cargo test --workspace`. Tests are a hard merge gate.
2. **Migration hygiene:** migration-safety/idempotency check (`PROC-CI-MIGRATION-HYGIENE-1`); secret scan (gitleaks) over the diff.
3. **Build + push:** build the container image, push to **GHCR**.
4. **Deploy:** on merge to `main`, `az containerapp update` to the new image (scale-to-zero). Single environment to start (this is a personal tool); a staging slot is optional and probably not worth it for one user.

Note `Camerata:ARCH-EXPAND-CONTRACT-1` for any later breaking schema change, and `PROC-AUTO-MERGE-1` only if Zach wants bot PRs to self-merge on green.

---

## 7. Risks, open questions, and one-way-door decisions

**One-way-door / architecture-level (need sign-off before Phase 2 starts the relevant slice):**
- **D1 — REST controllers vs Dioxus server functions.** The kickoff says "Controllers → Services → Repositories" (Agora REST), the spec says reuse the portfolio's Dioxus Fullstack deploy. `Camerata:RUST-DIOXUS-9` says fullstack apps use server functions, not a separate HTTP client. These produce different top-layer shapes (and a REST API is more surface than a single-user app needs). Recommendation: **Dioxus server functions calling services directly** (thinner, matches the deploy pattern, services stay the stable core), but this contradicts the literal "Controllers" wording, so it routes to Zach (`Agora:ROUTE-1`).
- **D2 — Month-boundary timezone** (ambiguity #5). UTC vs a fixed home timezone vs device-local decides which month a transaction belongs to. One-way-ish because it bakes into the rollover chain.
- **D3 — Category identity across budget versions** (ambiguity #6). Whether to add a stable `category_key` lineage now. Cheap to add up front, a migration + backfill later. Affects cross-version reporting and how rollover behaves across a version change.

**Integrity decisions (must be answered before the rolling-balance code is written):**
- **D4 — Unsettled flexible_set placeholders in the month-close net** (ambiguity #1).
- **D5 — Income's place in the rolling balance** (ambiguity #3): expense-only budget with income on the side, or income nets into Other.
- **D6 — Fund contributions as month expenses** (ambiguity #2): confirm `BUDGET-FUND-EARMARK-1`'s single-counting model is the intended semantics.
- **D7 — Buffer-financed full-price bookkeeping** (ambiguity #8): how the full price is recorded without blowing up its month.

**Lower-risk open questions (can be decided during the relevant Phase-2 slice):**
- Partial flexible_set settlement at close (ambiguity #9); Plaid `removed`-after-settlement cascade (ambiguity #10); forward-dated expected expense into a non-existent month (ambiguity #7); whether to show the conservative in-month Other view (`§10`); account granularity (`§10`); category archiving/reordering across versions (`§10`).

**Delivery risk (from the spec's own §11):** the backend/domain is checker-rich (compiler + tests + clippy) and will autonomously progress well; the **chorale UI integration is the checker-poor half** and needs manual QA and staying in the loop. The Phase-2 plan below front-loads the checker-rich work and isolates the UI as the human-in-the-loop slice.

**External dependency risk:** building against chorale `main` (pre-release, API churn expected and intended as dogfooding) means UI breakage will track chorale changes; acceptable per the spec, but the UI slice should not start until chorale v0.2.0's surface is stable enough to consume.

---

## 8. Proposed Phase-2 build plan & sequencing

Ordering front-loads the checker-rich layers (deterministic verification) and defers the checker-poor UI, per the spec's delivery-risk note and `Camerata:ORCH-ENV-GATED-QUALITY-1`. **Build-what-you-use** (`Camerata:SPIRIT-ROBUSTNESS-1`): design the schema complete, build only the active paths, stub the rest.

1. **Workspace + infra skeleton.** Cargo workspace + the six crates; Terraform (Neon + Azure + GHCR) provisioning an empty deploy; CI quality gate green on an empty app. Establishes the deploy spine before logic.
2. **Schema + entities + domain types (DESIGN-COMPLETE).** All tables from `§5` including every income mode, cadence, fund kind, and `repayment_obligations` — the full schema so no later migration is needed. SeaORM entities (`Agora:ENTITIES-*`), domain newtypes, the `Money` type (`BUDGET-MONEY-1`), error enums, repo traits, mappers. This is pure checker-rich work.
3. **Repositories + UnitOfWork.** Repo impls (`Agora:REPO-*`), `SeaOrmUow`/`SeaOrmUowProvider` (`Agora:REPO-10`), the status-inclusion and no-double-charge predicates as tested domain functions. Heavy unit-test coverage; the invariants in §3 get their conformance tests here.
4. **Month lifecycle + rolling Other (the core differentiator).** Lazy-init, multi-month catch-up, rollover transaction posting, idempotency (`BUDGET-ROLLOVER-INTEGRITY-1`, `BUDGET-IDEMPOTENT-MONTH-INIT-1`). Resolve D2/D4/D5 first. Property tests for cent-conservation and idempotency.
5. **Funds + large purchases + sinking funds.** `FundService`, earmark accounting (`BUDGET-FUND-EARMARK-1`), large-purchase resolution + repayment obligations (resolve D6/D7 first). Sinking-fund accrual + reset-on-payment.
6. **Income (BUILD semimonthly-fixed; STUB the rest).** Build `per_paycheck` for semimonthly/fixed (Zach's mode). **Stub** bi-weekly/weekly/hourly cadence resolution and the `smoothed` mode + smoothing-buffer logic behind clear seams (schema already supports them from step 2). Build surplus-routing default + per-transaction override (used in both modes).
7. **Auth + secrets.** TOTP auth reusing Agora's patterns (`Agora:AUTH-1/2`), Key Vault integration, single-user only (no multi-user code, `§9`).
8. **Plaid integration.** Plaid Link exchange, token→Vault (`BUDGET-PLAID-TOKEN-VAULT-1`), Transactions-only scoping, cursor sync + 30-day reconcile, sign normalization. Checker-rich except the Link widget.
9. **Initial-load seeding.** Per-category summary opening charges + starting balances from the reference xlsx.
10. **chorale transactions UI (the human-in-the-loop slice).** The grouped/aggregated/in-cell-editing transactions table on chorale (`§7`), via the chorale `main` git dependency. Built last, with manual QA; this is the checker-poor half and the dogfood. Optional hard-timeboxed 1-day pre-release grid slice if useful, dropped if it balloons.

**What to STUB explicitly (design-complete, build-what-you-use):**
- `smoothed` income mode + the income smoothing buffer (dormant for Zach; schema present).
- bi-weekly / weekly / hourly paycheck cadence resolution (only semimonthly built).
- Multi-user anything (schema is `user_id`-shaped; zero multi-user code, `§9`).
- The conservative in-month "Other" live view (`§4.3`/`§10`) unless Zach wants it.

---

## DECISIONS NEEDED BEFORE PHASE 2

**Blocking the relevant slice (must answer before that step builds):**
1. **D1 — Controllers vs Dioxus server functions** for the API boundary (kickoff says Controllers; spec says reuse the Dioxus Fullstack pattern; `Camerata:RUST-DIOXUS-9` favors server functions). My recommendation: server functions over services. *Routes to Zach per `Agora:ROUTE-1`.* — blocks steps 1 & 10.
2. **D4 — Do unsettled flexible_set placeholders count in the month-close net leftover?** — blocks step 4.
3. **D5 — Does income net into the rolling Other balance, or is the budget expense-only with income tracked separately?** — blocks step 4.
4. **D6 — Confirm fund contributions are recorded as month expenses (single-counting, `BUDGET-FUND-EARMARK-1`).** — blocks steps 4 & 5.
5. **D7 — How is a buffer-financed purchase recorded so the full price does not blow up its month?** — blocks step 5.

**Decide now because they are cheaper up front than as a later migration:**
6. **D2 — Month-boundary timezone** (UTC / fixed home TZ / device-local). — affects step 2 schema + step 4.
7. **D3 — Stable category identity across budget versions** (add a `category_key` lineage now?). — affects step 2 schema.

**Can be deferred to their slice (noted, not blocking):**
8. Partial flexible_set settlement at month-close (ambiguity #9).
9. Plaid `removed`-after-settlement un-settle cascade (ambiguity #10).
10. Forward-dated expected expense into a not-yet-created month (ambiguity #7) — likely `month_id` nullable + resolve at init, but confirm.
11. Plaid sign-convention normalization as a first-class rule (`BUDGET-PLAID-SIGN-1`) — recommend adopting.
12. Adopt the seven proposed `BUDGET-*` rules (§3) into the project's CONVENTIONS/Camerata set, and confirm whether they should be authored as Camerata principles (portable) or project-local rules.

---

*End of Phase 1 report. No application code, migrations, or Terraform were written. Phase 2 begins after this report is reviewed and the decisions above are made.*
