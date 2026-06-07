# Budget Tracker — Phase 1 Decisions (resolutions for the final resolved spec)

Resolutions to the open decisions in `PHASE_1_REPORT.md`. Fold these into `SPEC.md` to produce the
final resolved spec, and adopt the seven `BUDGET-*` rules (§3 of the report) as project-local rules.
All but **D5** are resolved; D5 is Zach's philosophy call (recommendation given).

---

## Blocking decisions

### D1 — API boundary: **Dioxus server functions (services retained).** RESOLVED.
Use **Dioxus server functions as the API/entry layer**, calling **services** directly; **repositories
stay**. The kickoff's "Controllers → Services → Repositories" wording is superseded only at the *thin
top layer*: server functions replace hand-written REST controllers + client fetch plumbing, they do
NOT replace the service or repository layers (per `RUST-DIOXUS-9`). The service + repo crate boundaries
(report §4) are unchanged. **This does NOT affect Agora** — Agora keeps REST controllers (a platform
wants a framework-agnostic API; a single-user monolith doesn't). Nothing in Agora is redone.

### D4 — Unsettled flexible_set placeholders at month-close: **count the placeholder.** RESOLVED.
At close, every category contributes `settled ? sum(settled txns) : placeholder` to the net leftover
(apply `BUDGET-NO-DOUBLE-CHARGE-1` consistently at the close step). An unsettled flexible_set counts
its **placeholder**. When the real bill settles later, the variance (actual − placeholder) reconciles
through the rolling chain and lands in the month the bill *settles* in. Accept the small timing
imprecision (variance hits the settling month, not the belonging month) — negligible for utilities,
and it keeps lazy-init/rollover timing simple.

### D5 — Income's place in the rolling Other: **B — income variance nets into Other (formula term).** RESOLVED.
*(Corrected 2026-06-07 after Zach's worked example; the earlier "recommend A" was a misread of his
model.)* The rolling Other reflects income variance. The month net leftover is:

> **net = (actual income − expected income) + Σ(expense category remaining)**

So a $100 income surplus (actual $5,850 vs expected $5,750) raises the net by $100, which raises Other
by $100 — **by formula, NOT as a discrete line item** (the Other number just comes out $100 higher).
This matches Zach's 4-year spreadsheet method exactly.

**No double-count:** surplus-routing and the smoothing buffer are a **smoothed-mode** feature. Zach is
**per-paycheck**, where income variance simply raises Other (no competing buffer), counted once. In
per-paycheck mode a higher-than-expected paycheck (e.g. the wellness reimbursement) auto-raises Other
with no checkbox needed — the wellness checkbox is a smoothed-mode-only override (divert surplus from
the buffer to this month). The earlier double-count concern only existed if smoothed-mode plumbing
fired in a mode Zach doesn't use; it doesn't.

### D6 — Fund contributions as month expenses (`BUDGET-FUND-EARMARK-1`): **CONFIRMED.** RESOLVED.
Yes — money moved into any fund (sinking accrual, surplus contribution, buffer repayment) is recorded
as an **expense against that month**, reduces free-to-spend, and is **excluded** from the net-leftover
that rolls into Other, so an earmarked dollar is counted exactly once. Fund *draws* are fund-draws, not
re-charged expenses (reuses `BUDGET-NO-DOUBLE-CHARGE-1`). Adopt the rule as written.

### D7 — Buffer-financed purchase bookkeeping: **full price recorded, ZERO month-budget impact.** RESOLVED.
The buffer-financed purchase posts the **full-price transaction for tracking**, but with **no impact on
its month's budget** — it's offset by the **buffer draw** (a fund-draw fronting the cash, not a
re-charge). The **repayment_obligation's monthly installments ARE the month-budget expenses** that flow
back into the buffer until `remaining = 0`. So: full price visible in history; monthly budget hit =
the installment only.

---

## Decide-now (cheaper up front than a later migration)

### D2 — Month-boundary timezone: **fixed home timezone = `America/New_York`.** RESOLVED.
Store all timestamps in **UTC** (per `ARCH-UTC-TIMESTAMPS-1`), but compute **month-membership by
converting to a fixed home timezone (`America/New_York`)**. This is travel-stable (his month stays his
home month whether he's in NYC or Europe) and intuitive. *(Zach: confirm `America/New_York` is the home
TZ you want as the default — trivial to change, it's a config value.)*

### D3 — Category identity across versions: **add `category_key` lineage NOW, defer the reporting.** RESOLVED.
Add a stable `category_key` (lineage id) column to `categories` now — it's one cheap column that
future-proofs cross-version reporting and avoids a painful migration + backfill later. Do **NOT** build
cross-version reporting in V1 (design-complete, build-what-you-use). The schema gets the affordance;
the feature waits.

---

## Lower-risk (resolved; build in the relevant slice)

- **Plaid sign convention (`BUDGET-PLAID-SIGN-1`): ADOPT.** Plaid uses positive = outflow; internal
  convention is negative = expense. Normalize (flip) at the **mapper boundary**, with a direction test.
- **Forward-dated expected expense into a not-yet-created month (#7): eager-create the target month.**
  When an expected expense targets a future month, create that month on demand (extend lazy-init to
  create-forward), keeping `month_id` non-null. Cleaner than a nullable FK + later resolution.
- **Partial flexible_set settlement at close (#9): still `pending` → counts the placeholder.** Real
  bills accumulate but don't count until all `expected_bills` have landed (then `settled` → sum). Same
  predicate as D4.
- **Plaid `removed`-after-settlement (#10): the removal cascade REVERSES settlement.** Un-settle the
  category (restore placeholder) / un-match the expected expense it had matched. Build in the Plaid
  slice.
- **"Exactly one rollover bucket" scope (#11): per budget VERSION** (one `is_rollover_bucket` per
  `budget_id`), enforced by a DB partial unique index.
- **Adopt the 7 `BUDGET-*` rules as PROJECT-LOCAL rules** (report §3). `BUDGET-MONEY-1` is an
  *elevation to mechanical enforcement* of the existing `ARCH-EXACT-DECIMALS-1` / `DOMAIN-8`. Promote
  any to portable Camerata principles later if they recur. Also fold in `BUDGET-PLAID-SIGN-1` and
  `BUDGET-SETTLE-ON-MATCH-1` (the report's two corollary candidates).

---

## Net for Phase 2
**All decisions now resolved (D5 = B, confirmed via Zach's worked example).** Phase-2 sequencing in
report §8 can proceed: front-load the checker-rich backend, isolate the chorale UI as the
human-in-the-loop slice built last against the chorale `main` git dependency.
