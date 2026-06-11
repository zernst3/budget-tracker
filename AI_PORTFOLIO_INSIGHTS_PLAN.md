# AI Portfolio Insights — Feature Plan (Budget Tracker extension)

> Build-agent brief. Hand this file to the coding agent **together with**
> `CONVENTIONS.md` + `AGENTS.md`. All existing project rules (`BUDGET-*`,
> `RUST-*`, `ARCH-*`, `ORCH-*`, `SQL-*`) apply unchanged. Implement in small
> vertical slices, phase by phase, tests first at the domain + reconciliation
> layers.
>
> **v2 — revised after architectural review.** The Plaid subsystem is the
> working precedent for nearly every piece here (external API behind a domain
> port, wire structs isolated in infrastructure, vault-backed secret, mock
> adapter with real fixtures, money-movement guard, audit-friendly entity).
> **Mirror Plaid; do not invent new patterns.**

## 0. Context & hard constraints

- **Single user (Zach).** No multi-tenant concerns.
- **Privacy:** positions only (ticker, shares, account label, value, optional
  cost basis). No account numbers / PII. No anonymization step required.
- **Runtime LLM = Google Gemini via the Gemini API** (key resolved through the
  existing `SecretVault` port — see S1). **NOT Anthropic** (per-call API billing
  belongs to a third party = off-limits; the Claude Code *subscription* used to
  *build* this is fine, runtime *API* calls are not). Provider is swappable
  behind the domain port.
- **Decouple ingestion from the feature.** v1 positions = manual entry (+ CSV if
  time allows). Plaid Investments auto-sync is a later, optional adapter behind
  the same `PositionSource` port.
- **The reliability/governance layer IS the deliverable** (and the interview
  artifact), not the model call. §4 is first-class.
- **No write surface, ever.** No broker order/trade/transfer API is integrated.
  Strictly easier than Plaid, which already had to guard `PlaidProduct::Transfer`
  via `assert_no_money_movement`; this feature has nothing to guard because it
  has no write path at all.

## 1. Architectural placement (mirror the Plaid subsystem)

| Layer (crate) | Adds |
|---|---|
| `budget-domain` | Types: `Position`, `CashBalance` (checking/buffer, flagged `reserved`), `NetWorth`, `PortfolioSnapshot` (carries per-position price + provenance + timestamp, **plus the cash/buffer balances and the computed net worth** — see §8), `Recommendation` (carries `claims: Vec<Claim>`), **`Claim` + `ClaimSubject` (the citable shape — see §2a)**, `ReviewRun`, `ValidationOutcome` (typed enum, see N1). **Ports:** `InvestmentAdvisor`, `MarketDataProvider`, `PositionSource`, `CashBalanceSource`. Mirror `plaid_api.rs::PlaidApi`. **NO http / Gemini / wire deps.** (`serde`/`serde_json` are already domain deps, so `String`/struct fields are fine — but **no `serde_json::Value` and no Gemini `responseSchema` type may appear on a domain trait**, M2.) |
| `budget-entities` + `budget-migration` | Table `review_runs` (system-managed log, see S3); `positions` table. SeaORM macros only (`RUST-ENTITIES-1..6`). Idempotent migration (`PROC-CI-MIGRATION-HYGIENE-1`); FK→`users` index in the same migration (`SQL-DB-INDEX-1`). |
| `budget-mappers` | entity ⇄ domain + domain ⇄ DTO mapping. |
| `budget-infrastructure` | Adapters: `GeminiAdvisor` (+ `advisor/wire.rs` holding all Gemini request/`responseSchema`/response structs, mirroring `plaid/wire.rs`); `<X>MarketData`; `CsvPositionSource`/`ManualPositionSource`. **Mocks:** `MockInvestmentAdvisor`, `MockMarketDataProvider` (S2). |
| `budget-ui` | `services/portfolio_review.rs` Dioxus **`#[server]` functions** (M1) returning **DTOs**, gated by `require_authed_user` (`gate.rs`); a `views/` Portfolio Review screen using `use_resource` (`RUST-DIOXUS-6`) + a read-only chorale `Table` (N4). |
| `budget-server` | **No changes.** (It only mounts the Dioxus router + auth layers.) |

`Money` (`rust_decimal`, no floats) backs all monetary values.

## 2a. The citable output shape (CANONICAL — lock this at the Phase-1 port sign-off)

The advisor does NOT return a wide struct of optional `referenced_*` fields. It
returns a list of **claims**, where each claim binds *what* is referenced to the
*number* asserted about it, and *what* is a **closed enum**. This is the shape
that makes the firewall mechanically enforceable (binding + coverage), and it is
the one-way-door decision being locked.

```rust
// budget-domain — pure types, no wire/Gemini deps.

pub struct Recommendation {
    pub title: String,
    pub rationale: String,
    pub claims: Vec<Claim>,        // the citable, reconcilable assertions
}

pub struct Claim {
    pub subject: ClaimSubject,     // WHAT is referenced
    pub cited_value: Money,        // the number the model asserts about it
}

/// Closed set of things a recommendation may cite. Extending the feature =
/// add ONE variant here; the `reconcile` match then fails to compile until the
/// new variant is given a ground-truth + reconciliation arm. Extensibility
/// lives in this enum, NOT in speculative struct fields.
pub enum ClaimSubject {
    Position { ticker: String },
    Buffer,
    NetWorth,
    // future: Sector { name }, CostBasisGain { ticker }, ... (one variant each)
}

pub enum ValidationOutcome {       // N1 — typed reason, not a bool
    Verified,
    Unverified(UnverifiedReason),
}
pub enum UnverifiedReason {
    UnknownTicker(String),
    ValueMismatch { cited: Money, ground_truth: Money },
    MissingMarketData(String),
}
```

**Why this shape (the binding + coverage argument, codified):**
- **Binding:** `Claim` pairs `subject` + `cited_value` as one unit, so a value is
  always reconciled against *its own* subject's ground truth. The "AAPL is
  $50,000 (actually NVDA's value)" tuple-mismatch is structurally impossible.
- **Coverage:** `ClaimSubject` is a closed enum reconciled by an exhaustive
  `match`. A new citable kind cannot be added without the compiler forcing its
  reconciliation arm — no ungoverned field, no silent firewall hole. The type
  system IS the lint rule here (cf. `ORCH-CONFORMANCE-1`: commitments are gates,
  not docs).

## 2b. Project rule — `BUDGET-AI-1` (exact, single-predicate; M3)

**Add via the Camerata principle selection + regenerate** (`CONVENTIONS.md` is
generated — do NOT hand-edit it). Camerata block format, phrased like
`BUDGET-NO-DOUBLE-CHARGE-1` ("decided by one predicate"):

> ### BUDGET-AI-1 — Every recommendation claim is tuple-reconciled against ground truth before display
> The sole enforcement site is `reconcile(rec: &Recommendation, snap:
> &PortfolioSnapshot) -> ValidationOutcome` in `budget-app-services`, an
> exhaustive `match` over `ClaimSubject`. A recommendation is `Verified` iff
> **every `Claim` resolves its `subject` to a single real ground-truth figure in
> `snap` and `cited_value` reconciles to THAT figure** within the band below;
> else `Unverified`. The subject and value are validated as one unit, never as
> independent set-membership. Unverified items are shown muted / never as fact;
> `Verified` is required to display a number as truth. **Tolerance (exact, no
> floats):** an absolute value reconciles iff `|cited − ground_truth| ≤
> Money::from_minor(1)` after `round_to_cents`; a cited percentage reconciles iff
> it equals `(subject_value / total).round(dp = N)` at displayed precision `N`.
> The `Decimal` band and `N` are pinned constants with a rationale (cf.
> `DeficitFinancingConfig`'s pinned `0.75`). Adding a `ClaimSubject` variant
> without a reconciliation arm is a compile error, by design. Enforcement test:
> the known-hallucination fixture (S2) must yield `Unverified`.

## 3. The feature (UX + behavior)

**"Portfolio Review"** screen:
1. **Positions table** (read-only chorale `Table`, like the ledger day-table in
   `views/ledger.rs`): ticker, account label, shares, price (+ provenance/
   staleness), market value, % of portfolio, optional cost basis. Manual
   add/edit = a form-per-row (`views/pending.rs` pattern), NOT editable cells.
2. **"Run Review"** button (debounced/disabled while in-flight, S6) → `#[server]`
   call → `GeneratePortfolioReview`.
3. **Insight cards**, each with: title, rationale, the **real numbers it's
   grounded in**, a **validation badge** driven by the typed `ValidationOutcome`
   (Verified / Unverified+reason), account-aware tax note (anchored to a
   deterministic rule, N2), Accept/Dismiss (local annotation only — review only).
4. **Standing disclaimer constant** (N3): "Informational only, not licensed
   financial advice."

## 4. Reliability / governance layer (the core deliverable)

1. **Grounding** — prompt built only from the real `PortfolioSnapshot` + fetched
   market data + account-type context; model instructed to reason only over
   provided data and to emit its assertions as `Claim`s (a `ClaimSubject` + the
   `cited_value` it asserts), never as free-text numbers. The `responseSchema`
   mirrors the `Claim`/`ClaimSubject` shape so the wire output maps 1:1 to it.
2. **Forced JSON** — Gemini `responseMimeType: application/json` + `responseSchema`
   (structs in `advisor/wire.rs`). `responseSchema` reduces but does not
   eliminate malformed output → the adapter has a parse-failure path that records
   the raw string to the audit row and returns a typed `AdvisorError` (model the
   enum on `PlaidError`).
3. **Claim reconciliation (BUDGET-AI-1)** — the single `reconcile(...)` predicate:
   an exhaustive `match` over each `Claim`'s `ClaimSubject` that resolves the
   ground-truth figure and compares `cited_value` to *that* figure (subject+value
   as a unit, M4). New `ClaimSubject` variants can't compile without a
   reconciliation arm (§2a).
4. **Terminal states (M4)** — define and test each: *malformed JSON* → typed
   error + audit row; *zero verifiable recs / empty model output* → explicit
   "no verifiable insights this run" SUCCESS state, `ReviewRun` still persists;
   *failed/stale quote* → that position has no ground-truth price, so any rec
   citing it is `Unverified` **by construction**; *empty portfolio* →
   short-circuit before the model call.
5. **Guardrails** — no fabricated tickers/prices (caught by §4.3); scope limited
   to portfolio observations + tax placement; disclaimer always attached;
   low-confidence flagged. **The cash buffer is grounded as `reserved /
   non-investable` (§8): the advisor must NOT recommend deploying it and must NOT
   treat it as "cash drag."** If a rec cites buffer or net-worth figures, those
   are reconcilable fields too (tuple-reconciled like positions).
6. **Human-in-the-loop** — display for review only; the app NEVER trades or moves
   money; no broker write API exists.
7. **Observability / audit** (`review_runs`, S3) — persist per run: model id +
   version, prompt hash, raw output string, the grounding snapshot, per-rec
   `ValidationOutcome`, token/cost counters, latency, `occurred_at`.
8. **Eval harness (Phase 7, optional)** — synthetic portfolios with known issues;
   assert detection + correct reconcile pass/fail.

## 5. Tech notes

- **S1 — Secrets via `SecretVault`, not env.** Resolve the Gemini + market-data
  keys through the domain `SecretVault` port (`AzureKeyVault` prod /
  `InMemorySecretVault` mock), with an `AI_MODE=mock` opt-in mirroring
  `PLAID_MODE=mock` in `server_state.rs`. Local dev + CI run with zero network,
  zero real key (`ORCH-ENV-GATED-QUALITY-1`, `ARCH-IAC-1`).
- **S4 — Gemini resilience + model id as config.** Request timeout; typed
  `AdvisorError::{RateLimited, Unavailable, Parse, ...}`; bounded retry. **Do not
  hard-code model ids / free-tier limits as fact** — `gemini-*` ids and limits
  are externally-moving (`ORCH-TRAINING-CUTOFF-1`); make the model id config and
  confirm the current id at build time. Free tier ≈ $0 at single-user volume,
  but verify.
- **S5 — Concurrent market-data fan-out.** N quote fetches are independent →
  `try_join_all`/`futures::join`, never sequential `await` in a loop
  (`ARCH-PARALLEL-INDEPENDENT-1`).
- **Market data:** one free API (Finnhub / Twelve Data / Alpha Vantage) behind
  `MarketDataProvider`, with a **manual-price fallback** so a position always has
  a ground-truth price and the feature is never blocked on the API choice.
- **No floats** in money math; percentages computed for display only.

## 6. Build phases (resequenced — firewall before network)

- **Phase 0 — green the base app.** Test/finish the existing app first; never
  extend a red build.
- **Phase 1 — domain + persistence.** Types + ports (mirror `PlaidApi`);
  `review_runs`/`positions` migrations + entities + mappers. Unit-test domain.
  **Pause for Zach's sign-off on the `InvestmentAdvisor` port surface
  (`ORCH-ONE-WAY-DOOR-1`).**
- **Phase 2 — ingestion.** `ManualPositionSource` (+ `CsvPositionSource` if time);
  read-only positions table UI.
- **Phase 3 — market data.** `MarketDataProvider` port + manual-price fallback
  FIRST; then one real adapter. (Sneaky schedule risk — de-risk early.)
- **Phase 4 — advisor port + MOCK.** `InvestmentAdvisor` port + `MockInvestmentAdvisor`
  driven by captured fixtures, including the deliberately-hallucinated fixture.
- **Phase 5 — reliability engine (longest pole).** `reconcile(...)` + tuple
  matching + tolerance + terminal states + audit logging, built and tested
  **entirely against the mock, no network.** Hallucination-fixture test required.
- **Phase 6 — real `GeminiAdvisor` + UI.** Wire real Gemini last; Portfolio
  Review screen (`#[server]` + DTOs + chorale read-only table + badges).
- **Phase 7 — later/optional.** `PlaidPositionSource`; weekly auto-run + email;
  eval harness.

**Safe v1 cuts if time is tight:** all of Phase 7; CSV → manual-only;
sector/tax insights → concentration + cash-drag only. **Do NOT cut:** forced
JSON, tuple reconciliation, the audit row, the hallucination-fixture test —
those *are* the deliverable.

## 7. Definition of done (v1 = Phases 0–6)

- Positions enter via manual (/CSV); market values resolve (or manual-price).
- "Run Review" returns recommendations, each grounded in real numbers.
- Every recommendation is **tuple-reconciled**; unverified items visibly flagged,
  never shown as fact (BUDGET-AI-1 enforced + hallucination-fixture test green).
- Empty / stale / partial / malformed terminal states all defined and tested.
- No write/trade path exists anywhere; review-only human gate.
- Each run persisted + auditable; provider swappable via the domain port.
- Local/CI runs offline via `AI_MODE=mock` + fixtures.
- Net worth + buffer surface correctly (§8) and never leak into budget totals
  (`BUDGET-CASH-1`).

## 8. Refinement — cash, the invisible buffer, and net worth

Two different kinds of thing that must NOT mix (flows vs. balances):

- **Flows = budget.** Checking-account *transactions* (deposits = income,
  withdrawals = spending) feed the budget ledger + category math. Existing app.
- **Balances ≠ budget.** The *cash sitting in checking* is the invisible buffer
  (the §4.9 reserve). It is **display-only**: shown as a reminder, but it NEVER
  enters budget-category aggregation. Net worth is also a balance concept.

**New rule (Camerata format, house style — single boundary):**

> **BUDGET-CASH-1 — Cash/checking balances are buffer + net-worth display only.**
> A checking *balance* MUST NOT enter any budget-category aggregation; only
> checking *transactions* (deposits/withdrawals) do. Balances reach `NetWorth`
> and the buffer display via one path; category totals via another; they never
> cross. Enforcement: balances are typed `CashBalance`, distinct from the
> transaction/ledger types, and no budget aggregation function accepts a
> `CashBalance`.

**Behavior:**
- **Buffer display:** each checking balance + a total "Buffer: $X — reserved, not
  budgeted" line. Read-only.
- **Net worth = `sum(CashBalance) + sum(position market value)`.** **v1 =
  assets-only** (all current accounts are assets). Liabilities (card balances,
  loans) are a later subtraction if Zach wants them — flag, don't assume.
- **Source:** `CashBalanceSource` port (manual entry first; Plaid `/balance` as a
  later adapter, same decoupling as positions). `Money`/no-floats throughout.

**Why this is in THIS plan (port-surface / one-way-door tie-in, M2):** the
`PortfolioSnapshot` the advisor sees now includes the buffer balance (flagged
`reserved`) and net worth, and the advisor may cite them — so those become
**citable, reconcilable fields in the snapshot AND `RawRecommendation`.** Lock
this field list at the Phase-1 port sign-off; adding a reconcilable figure after
the port locks means changing the trait + wire + schema + `reconcile()` +
fixtures + tests.
