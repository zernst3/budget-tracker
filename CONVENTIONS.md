# Budget Tracker — Project-Local Rules (`BUDGET-*`)

Project-local Camerata-format rules adopted for the budget tracker (Phase 1, 2026-06-07). These extend
the portable Camerata library and the Agora-rs CONVENTIONS; cite them by ID in commit messages
(`PROC-CITE-CONVENTION-ID-1` / `CC-2`). Promote any to portable Camerata principles later if they recur.

Domain: `budget`. Each rule carries the standard Camerata fields plus a `qualifies` conformance test.

---

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
qualifies = "A clippy/CI check (disallowed-types lint or a grep gate) fails the build if f32/f64 appears in any type whose name or column maps to money. A property test asserts that summing N transactions and rolling the balance forward M months loses zero cents versus a Decimal oracle."
```

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
settlement and expected-expense matching.
"""
why = """
This is the SPEC §4.5 invariant and the root of correctness for fixed expenses. Implemented as a flag \
or duplicated across call sites it WILL drift, double-counting rent the first time someone assigns a \
pulled transaction. As a single shared predicate it is enforceable and reusable across the three \
settlement surfaces (fixed, flexible_set, expected) so they cannot diverge.
"""
alternatives = [
  "A boolean has_real_txn flag toggled per category (rejected: flags drift from the underlying truth and invite the both-counted bug)",
  "Subtract the placeholder when the first real txn lands (rejected: equivalent result, more moving parts, breaks on multi-bill flexible_set)",
]
qualifies = "Unit test: create a fixed category with a placeholder, assign a real transaction, assert spent equals the real transaction (not placeholder + transaction). Second test: an unsettled category's spent equals the placeholder exactly. The spent computation exists in exactly one function (verified by grep/review)."
```

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
is_rollover=true, category=the rollover bucket, dated the 1st, amount = the prior month's net leftover \
(net = (actual income - expected income) + sum(expense category remaining); fund contributions are \
already expensed and excluded). The rolling balance is always the sum of this auditable chain; it is \
never stored as a mutable scalar. Posting a month's rollover is idempotent: a (month, is_rollover) \
uniqueness guard makes re-running lazy-init a no-op. Multi-month catch-up posts each missed month's \
rollover in chronological order.
"""
why = """
The rolling Other bucket is the entire reason the app exists (SPEC §4.3); its integrity is \
non-negotiable. A mutated number is unauditable and irrecoverable if a calculation is ever wrong. A \
transaction chain is inspectable ('April rollover: +$212'), reconstructable, and naturally idempotent \
under a uniqueness constraint, which is exactly what lazy-init on a scale-to-zero container requires \
(SPEC §4.6).
"""
alternatives = [
  "Store a running balance column on months and mutate it (rejected: unauditable, not idempotent, corrupts permanently on a single bad write)",
  "Recompute the whole chain from genesis on every load (rejected: unbounded work as history grows; the transaction chain already is the recomputation, persisted)",
]
qualifies = "A test runs lazy-init across a simulated 3-month gap and asserts exactly 3 rollover transactions exist with correct sequential amounts; running it again posts zero additional rows. A DB partial unique index on (month_id) WHERE is_rollover enforces single-posting."
```

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
backup, or log leak into account-history exposure. The read-only product scoping (SPEC §6) makes the \
token physically incapable of moving money; the Vault-reference rule keeps even the read token out of \
the blast radius of a database compromise. Mirrors Agora's Key Vault secret-reference pattern.
"""
alternatives = [
  "Encrypt the token at rest in the DB with an app-held key (rejected: the app process holding both ciphertext and key collapses to plaintext on compromise; Key Vault separates custody)",
]
qualifies = "A CI grep/secret-scan gate fails if a Plaid token literal or an access_token column write is present. A test asserts plaid_items has no raw-token column and that the Plaid client reads from the Vault. A startup assertion confirms only Transactions/Accounts products are configured."
```

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
settled = included, expected = included (it reserves budget, SPEC §4.10), pending = excluded \
(SPEC §4.4). Every aggregation (category spent, month net leftover, free-to-spend) routes through this \
predicate. The opposite treatment of Plaid 'pending' (excluded) and manual 'expected' (included) is \
encoded once.
"""
why = """
Pending-excluded and expected-included are deliberately opposite handlings of the same status column \
(SPEC §4.10). If the inclusion test is reimplemented per aggregation, one site will inevitably get the \
polarity wrong and either reserve budget for transient pending charges or fail to reserve it for known \
commitments. One predicate makes the polarity a single, testable decision.
"""
alternatives = [
  "Filter status inline at each query (rejected: N copies of the polarity, guaranteed drift)",
  "Two columns (counts_in_budget bool + status) (rejected: redundant state that can contradict the status)",
]
qualifies = "A test enumerates all three statuses and asserts inclusion=true/true/false for settled/expected/pending against the single predicate. Grep confirms aggregations call the predicate rather than inlining a status filter."
```

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
This resolves the most dangerous interaction gap in the spec: funds and the rolling balance both claim \
'free-to-spend' semantics, and without an explicit single-counting rule an earmarked dollar inflates \
Other and gets spent twice. Making fund contribution an explicit month expense gives the \
virtual-envelope discipline (SPEC §4.7/§4.9) and keeps the rollover honest.
"""
alternatives = [
  "Track funds entirely outside the budget and let free-to-spend ignore them (rejected: removes the discipline that is the whole point of a virtual envelope)",
  "Net funds into the rollover instead of expensing them monthly (rejected: makes the rollover a mixed signal and hides fund health)",
]
qualifies = "A test: accrue $50 into a sinking fund in a month with otherwise $0 net; assert month net leftover is -$50 (not +$50 or $0) and the fund balance is +$50, so total system money is conserved. A conservation property test asserts sum(category remaining) + sum(fund balances) is invariant under a fund contribution."
```

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
BUDGET-ROLLOVER-INTEGRITY-1) and resolving the correct budget version per month. Month-membership is \
computed in the fixed home timezone (America/New_York); timestamps are stored UTC. Expected expenses \
targeting a future month eager-create that month. The whole operation is idempotent and \
concurrency-safe: re-entry (including two requests racing on container wake) creates no duplicate \
months or rollovers. A midnight timer is at most an opportunistic backstop, never the source of truth.
"""
why = """
On a scale-to-zero container the timer cannot be relied on (SPEC §4.6); lazy-on-access is the only \
correct mechanism, and correctness there means surviving multi-month gaps and concurrent wake-up \
requests without double-posting. The UNIQUE(user_id, year, month) constraint plus the rollover \
uniqueness guard make this enforceable rather than hopeful.
"""
alternatives = [
  "Rely on a scheduled timer to roll months (rejected by SPEC §4.6: the container is asleep when it should fire)",
  "Initialize only the current month, skipping gaps (rejected: a 2-month absence would lose a rollover link and break the chain)",
]
qualifies = "Tests: (a) cold app with last month = March, current = June -> three months created with linked rollovers; (b) calling init twice yields identical state; (c) two concurrent init calls (simulated) produce one set of rows (relies on UNIQUE + ON CONFLICT)."
```

```toml
id = "BUDGET-PLAID-SIGN-1"
title = "Plaid amounts are normalized to the internal signed convention at the mapper boundary"
tag = "stack"
domain = "budget"
layer = "framework"
enforcement = "structured"
default = true
summary = """
Plaid reports amounts as positive for outflows (debits). The internal convention is signed with \
negative = expense, positive = inflow. The flip happens exactly once, at the Plaid mapper boundary in \
the mappers crate, before any transaction enters the domain. No downstream code re-interprets Plaid \
sign.
"""
why = """
Sign-direction bugs are silent and catastrophic in a budget tracker: a flipped sign turns a $200 \
expense into $200 of income and corrupts every aggregate and the rolling balance. Centralizing the \
flip at the single ingestion boundary makes the convention one tested decision instead of a \
per-call-site guess, and keeps the rest of the system reasoning in one consistent sign.
"""
alternatives = [
  "Keep Plaid's sign and special-case it in aggregations (rejected: spreads Plaid's convention through the whole system, guaranteeing a missed site)",
]
qualifies = "A mapper test feeds a Plaid debit (positive) and asserts the resulting domain transaction amount is negative, and a Plaid credit/refund (negative) maps to positive. Domain/aggregation code contains no Plaid-sign handling (verified by review)."
```

```toml
id = "BUDGET-SETTLE-ON-MATCH-1"
title = "Placeholder settlement is one shared match-and-replace operation across flexible_set and expected expenses"
tag = "stack"
domain = "budget"
layer = "library"
enforcement = "structured"
default = true
summary = """
Settling a placeholder against a real transaction (flexible_set categories per SPEC §4.2, manual \
expected expenses per SPEC §4.10) goes through one shared match-and-replace function: the real \
transaction REPLACES the placeholder's budget effect, never adds to it (reuses \
BUDGET-NO-DOUBLE-CHARGE-1). The same operation drives the reverse path when a matched transaction is \
removed (Plaid 'removed'), restoring the placeholder / un-matching the expected expense.
"""
why = """
flexible_set settlement, expected-expense matching, and Plaid-removed reversal are the same \
placeholder-until-a-real-charge-replaces-it mechanic applied to three surfaces. Implemented separately \
they will drift and reintroduce the double-count bug on at least one surface. One match-and-replace \
operation makes the settlement semantics a single tested thing reused everywhere.
"""
alternatives = [
  "Implement settlement independently per surface (rejected: three copies of the no-double-charge logic, drift guaranteed)",
]
qualifies = "A test matches a real transaction to an expected-expense placeholder and asserts the category's spent reflects the real txn only (not both); a second test does the same for a flexible_set placeholder; a third removes a matched txn and asserts the placeholder is restored. All three call the same function."
```

```toml
id = "BUDGET-CUTOVER-1"
title = "tracking_start_date is the genesis boundary; the pre-day-1 world is a settled opening snapshot, never tracked or ingested"
tag = "stack"
domain = "budget"
layer = "library"
enforcement = "structured"
default = true
summary = """
Onboarding is anchored to an explicit users.tracking_start_date ('day 1'). Everything dated before it \
is CLOSED and is represented solely by the onboarding opening snapshot: per-category month-to-date \
summary opening charges for the partial first month plus the correct starting Other/buffer balances, \
computed as of the end of the day before day 1 (capturing any spend in the lock gap between the last \
spreadsheet day and day 1). No per-transaction record is created for the pre-genesis world, and Plaid \
sync plus the rolling 30-day reconcile exclude any transaction dated before tracking_start_date (the \
reconcile lower bound is clamped to max(today - 30 days, tracking_start_date)).
"""
why = """
A clean cutover from the spreadsheet needs one unambiguous boundary, or the opening snapshot and the \
first Plaid pull will both claim the same pre-day-1 spend and double-count it, corrupting the rolling \
balance from the very first day. Making day 1 an explicit stored date, declaring the prior world \
closed, and hard-excluding pre-genesis Plaid rows reuses the no-double-charge spirit (the opening \
snapshot IS the placeholder for all prior history) and makes onboarding correctness a single tested \
boundary rather than an emergent accident of when the first sync happens to run.
"""
alternatives = [
  "Backfill full history from Plaid and skip the opening snapshot (rejected by SPEC §4.6: expensive and unnecessary; the rolling balance only needs a correct starting point)",
  "Use the setup timestamp implicitly as day 1 with no stored date (rejected: not reproducible, cannot represent a chosen cutover or a lock gap, and gives the Plaid guard no boundary to filter on)",
]
qualifies = "A test sets tracking_start_date and feeds a Plaid sync containing transactions both before and after it; asserts only on-or-after rows are ingested and the pre-date rows are dropped. A second test asserts the onboarding opening snapshot plus post-day-1 transactions reconstructs the same balance as the spreadsheet's day-0 state, with no pre-day-1 transaction rows present."
```
