# Budget Tracker — Project Spec

> **Status: PHASE-1 RESOLVED / build-ready** (sequenced after chorale v0.2.0 + the portfolio site). Captured 2026-06-05; design extended 2026-06-07 (income model §4.8, sinking funds §4.7, buffer/surplus/large-purchase model §4.9, revised infra §8 to Neon-free, dogfood sequencing §7). **Phase-1 decisions folded in 2026-06-07 — see §12 (authoritative resolutions). The project ruleset is emitted in Camerata format to `CONVENTIONS.md` (structured/mechanical) and `AGENTS.md` (prose), including the 11 project-local `BUDGET-*` rules.**
> **V1 is tailored ENTIRELY to Zach (sole user).** His personal compulsory-repay-buffer model (§4.9) is encoded as-is; broader customizability for other financial styles is explicitly deferred.
> **Sequencing:** build AFTER (1) chorale v0.2.0 ships and (2) the new Rust portfolio website,
> both of which are higher priority because they're job-search-relevant. This is a personal-life
> tool, not job-search work. Intended to run as a background-routine build once those are done.
> **Why front-loaded this thoroughly:** so a background routine + the Camerata Rust rules can build
> it with minimal mid-stream oversight. Every decision below was made deliberately; treat them as
> settled requirements unless Zach revisits.

---

## 1. Purpose & origin

Replaces a 4-year-old Google Sheets budget (tracked to the dollar). The sheet's core logic was
reverse-engineered from its formulas (see §2). The reason to BUILD rather than use an off-the-shelf
app (YNAB, Monarch, Lunch Money): none of them implement Zach's custom **rolling "Other" bucket
reconciliation** (§4). That custom rule is the entire justification for a bespoke app.

It's also the ideal **dogfood for chorale** (Zach's own Rust/Dioxus table library) — the main screen
is a transactions table with grouping/aggregation and in-cell editing, which is exactly chorale
v0.2.0's feature set (§7).

## 2. The original spreadsheet model (context, reverse-engineered)

Per-month sheet structure:
- Columns = ~22 spending categories (Investments, Rent, Utilities, Internet, Insurance,
  Transportation/Gas, Car Expenses, Groceries, Spotify, Amazon Prime, Fun/Entertainment, Phone, Gym,
  Donations, Domestic Travel, 4× International Travel sub-buckets, Other). Plus Total + Notes.
- Category NAMES defined centrally; budget AMOUNTS pulled from a separate "budget config" sheet
  (e.g. 'New York City Budget'). This is already a clean budget-config-vs-tracking separation.
- Rows = dates; each cell = the SUM of that category's spend for that day, entered as inline
  arithmetic (`=-5.98-17.39-202.63`). **This loses per-item detail, exact purchase dates, and which
  account** — the upgrade (§3) fixes exactly this.
- "Total per month" row = sum of daily rows per category. "Remainder" row = budget + spent
  (positive = underspent, negative = overspent).
- A "Last" note (e.g. "Last was Payment from Check 9428 June 3 500") = the bookmark for where he
  stopped tracking.
- The "Other" bucket budget used an asymmetric formula:
  `Other = base + (full remainder of FIXED buckets) + (only the OVERSPEND, IF(rem<0), of
  DISCRETIONARY buckets)`. This was a **spreadsheet HACK** to approximate a rolling balance within a
  single sheet (a sheet can't carry state across tabs). The app replaces it with a real rolling
  balance (§4) — do NOT port the asymmetric formula.

## 3. The upgrade (core requirement)

Move from "daily-inline-sum-per-category" to **individual transaction records**, each with: date
(actual purchase/post date), amount, name/description, account, category. Monthly per-category totals
become aggregations over the transactions table. This is what forces a DB + fullstack app.

## 4. Functional requirements

### 4.1 Budgets are customizable & versioned
- No hardcoded categories. Categories are DB items the user defines.
- Budgets are **versioned** (effective date range), mirroring how the sheet evolved
  ("2022 Budget" → "NYC Budget" → "Polish Budget" as life changed). A month REFERENCES the budget
  version active for it (FK, not a copy). Editing the budget creates/advances a version; past months
  keep their referenced version so history stays accurate.

### 4.2 Category model
Each category has:
- `name`, `amount` (monthly budgeted amount)
- `grp`: **`fixed` / predictable** OR **`discretionary`**
- `settle_type` (only for fixed): **`true_set`** or **`flexible_set`**
  - **true_set** (rent, phone): amount known in advance & stable; effectively settled at month start.
    Overspend is rare but allowed (e.g. travel) and reconciles to Other like everything else.
  - **flexible_set** (utilities): budget is a PLACEHOLDER; actual unknown until the bill(s) land.
    Budgeted-counts-as-spent until settled. Carries a state: `pending` → `settled`.
- `expected_bills` (int, flexible_set only): how many actual transactions must be assigned before the
  category is considered fully settled. **Customizable per category.** Example: Utilities = 2 (Zach
  pays separate electricity + gas; only settle once BOTH are tracked). A manual "mark settled" button
  is also allowed.
- `is_rollover_bucket` (bool): exactly ONE category is the rollover bucket ("Other").
- `cadence`: **`monthly`** (default) OR longer (**`quarterly` / `semiannual` / `annual`**, or arbitrary
  `period_months`). Monthly = normal reconciled category. Longer-than-monthly = a **sinking fund**
  (§4.7). A hybrid real-world item (gym = monthly fee + annual fee) is modeled as **two categories**
  (one `monthly`, one `annual`), optionally grouped under a display parent.

### 4.3 The rolling "Other" bucket (the custom rule — the whole reason to build)
- "Other" is a **rolling balance**. On the 1st, the previous month's **total net leftover** (across
  ALL categories, including whether he was over/under on the TOTAL budget) becomes a line item in the
  new month's Other.
- **Implemented as its own transaction**, NOT as a mutated budget number: a system-generated
  `transactions` row with `is_rollover=true`, category = the rollover bucket, amount = prior month's
  net (positive surplus or negative overspend), dated the 1st. This makes the rollover **auditable**
  ("April rollover: +$212" visible in Other) instead of a silently-shifting figure.
- **Income variance is part of the net (D5 = B, RESOLVED 2026-06-07; see §12).** The month net is:
  `net = (actual income − expected income) + Σ(expense category remaining)`. A higher-than-expected
  paycheck raises Other by the surplus **by formula** (the Other figure simply comes out higher), not
  as a discrete line item — matching the 4-year spreadsheet. In per-paycheck mode (Zach's) there is no
  competing smoothing buffer, so income variance is counted **exactly once**. (Surplus-routing / the
  smoothing buffer are smoothed-mode-only, §4.8; they never co-fire with this in per-paycheck mode.)
- The spreadsheet's in-month asymmetry (overspend counts live, underspend only at month-close) was a
  conservative live-display choice; the underlying truth is just the carryover balance. The app can
  optionally show a conservative live view but the source of truth is the rolling balance.

### 4.4 Pending vs settled
- Bank (BoA credit card) transactions can be `pending` or `settled`. **Pending are NOT expenses until
  settled** — store them but EXCLUDE from budget math until `status='settled'`.

### 4.5 No double-charging fixed expenses
- Rule (not a flag): a fixed category's spent = **`settled ? sum(its transactions) : budget-placeholder`,
  never both.** While unsettled, the budget acts as the placeholder; once real transactions are
  assigned they REPLACE it. So assigning a pulled "rent" transaction settles it against the
  placeholder instead of adding on top.

### 4.6 Month lifecycle
- Months are DB items (`open` / `closed`), referencing a budget version (§4.1).
- **Month init is LAZY — and on a scale-to-zero container, lazy is the ONLY reliable option.** When
  the app loads, if the current month doesn't exist, create it (reference current budget) and post the
  prior month's rollover. **Zach's cold-start question (2026-06-07), answered:** a midnight-on-the-1st
  timer is NOT reliable on the free / scale-to-zero container — the container is asleep when there's no
  traffic, so the timer simply never fires. That's fine, and it's exactly why lazy-init is the source
  of truth: the NEXT time the app is opened (which wakes the container), lazy-init runs and **catches
  up**. It MUST handle a **multi-month gap** (loop through every missed month in order, posting each
  rollover sequentially) and be **idempotent** (re-opening never double-posts). So drop the timer as a
  primary mechanism; at most keep a tokio task as an opportunistic backstop for when the container
  happens to already be running. Lazy-on-access is what actually guarantees correctness.
- **Initial load / onboarding (DECIDED 2026-06-07; cutover model clarified — see §12 D8):** do NOT
  import full transaction history. Onboarding is anchored to an explicit **`tracking_start_date` (your
  chosen "day 1")**: everything dated before it is considered **CLOSED** and is never tracked
  per-transaction. On setup, seed the month containing day 1 by writing, **per category, a single
  summary opening charge** = that category's running total for the partial first month, plus the
  correct STARTING buffer/Other balance (§4.3, §4.9), all taken **as of the end of the day before day
  1** — so any spend in the **lock gap** between your last spreadsheet day and day 1 is captured in the
  opening numbers. From there, track normally going forward (real per-transaction records accumulate on
  top of the opening summary lines). **Plaid never ingests transactions dated before
  `tracking_start_date`** — they are already represented in the opening snapshot, so pulling them would
  double-count (see §6 and `BUDGET-CUTOVER-1`). Cheap, gets him live on day 1, and the rolling balance
  only ever needed a correct starting point, not four years of history.

### 4.7 Sinking funds (the periodic / non-monthly-expense rule)
Expenses paid less often than monthly (Amazon Prime = annual, old car insurance = 6-month, the gym's
annual fee) must NOT blow up the month they land. The standard, correct primitive (how YNAB / accrual
budgeting do it) is a **sinking fund**: set aside `amount ÷ period_months` every month into a
**carryover fund**; when the real bill hits, pay it FROM the accumulated fund, not that month's cash.

- A category with `cadence > monthly` IS a sinking fund. Monthly accrual = `amount / period_months`.
- The fund balance **carries over** month to month (unlike a normal category that resets).
- **Virtual envelope:** the money physically sits in the real checking account; the fund only
  EARMARKS it. So accrued fund balances **reduce free-to-spend** — that IS the discipline (stops the
  renewal/insurance savings from being spent). Same carry-forward primitive as the income smoothing
  buffer (§4.8), run in the saving direction.
- **Reset-on-payment (resolves "you pay for the FUTURE, not the past"):** when the real payment lands,
  the user tags that transaction as **the sinking-fund payout**. That (a) draws the reserve down and
  (b) **resets the accrual clock** — from that date, accrue toward the NEXT occurrence. Accrual is
  forward-looking, anchored to the last actual payment. So the "spread this charge over the next 12
  months" instinct = reset + re-accrue forward.
- **Hybrid items (gym):** two categories — a `monthly` recurring + an `annual` sinking fund;
  reconcile independently.
- **Mid-cycle setup:** a fund needs `next_due_date` + `current_balance` so it computes the right
  monthly accrual when started partway through a cycle (catch up faster, or seed it).

### 4.8 Income (NEW area — the spreadsheet had none)
Money INTO checking is tagged **budgeted income** (expected recurring paychecks; reconciles against an
expectation like a flexible_set expense and resolves at month-end) or **new income** (unplanned inflow:
gift, refund, bonus, side gig; pure addition, no prior expectation).

**Two calculation modes (a per-user setting — they map to financial SITUATIONS, not taste):**
- **`per_paycheck` (EXACT — the DEFAULT; fewest preconditions):** expected income this month =
  `(paychecks landing this month) × amount`, computed per-month from the cadence. **No averaging → no
  long-term balance to track**; a 3-paycheck month simply has a higher expectation that month. Works
  for users WITHOUT a savings buffer, and for **hourly / variable** income (leave `amount` blank → it
  degrades to pure actual-tracking, no expectation).
- **`smoothed` (12-month average — OPT-IN; requires a buffer):** budgeted monthly =
  `(amount × paychecks_per_year) ÷ 12`, constant every month. Needs an **income smoothing buffer**:
  3-paycheck-month surplus accumulates in the buffer, 2-paycheck months draw from it, nets to zero
  over the year. The buffer **reduces free-to-spend** (carry-forward primitive again — a sinking fund
  in REVERSE) — which is also the discipline that stops "bonus" months from being blown. **SAFETY:**
  smoothed in the hands of a buffer-less user is dangerous (it budgets money that hasn't arrived yet)
  → surface that it assumes a buffer; do NOT present it as a neutral toggle. The buffer may dip
  slightly negative early in a cycle before the first surplus month replenishes it; seed it or allow a
  small negative.

**Paycheck config (schema):**
```
paycheck_config               -- income setup (one per user; extension-ready)
  id, user_id→users,
  income_mode ('per_paycheck'|'smoothed') default 'per_paycheck',
  paycheck_type ('semimonthly'|'biweekly'|'weekly'|'hourly'),  -- semimonthly=24/yr always 2/mo;
                                                               -- biweekly=26/yr 2-3/mo; weekly=52/yr 4-5/mo
  amount numeric?,            -- null/blank = hourly/variable (actual-tracking)
  anchor_date date,           -- next/last paycheck → app infers paychecks-per-month
  surplus_routing ('buffer'|'this_month'|'savings') default 'buffer',  -- default for over-expected income
  smoothing_buffer numeric default 0          -- income smoothing buffer (smoothed mode)
```
Income flows reuse the `transactions` table (positive amount = inflow) with an `income_kind`
(`budgeted` | `new`) flag.

**Income surplus routing (GENERAL — both modes):** when an actual deposit exceeds expected (Zach's
"wellness reimbursement" arriving inside a paycheck; overtime; a bonus), the over-amount is a positive
delta that must be ROUTED. Provide the `surplus_routing` **default rule** PLUS a **per-transaction
override** — Zach's checkbox: "this paycheck was higher than budgeted, add the extra to THIS month."
NOT smoothed-only; over-expected income happens in either mode.

**Zach's own situation:** **semimonthly = always exactly 2 paychecks/month**, so for HIM both modes are
identical and the buffer never fires. The dual-mode + buffer + cadence machinery is the **extension
for future bi-weekly / weekly / hourly users** (incl. himself if a new job changes his pay).

**Build discipline (job-search future-proofing, done RIGHT):** DESIGN the schema for all modes
(`income_mode`, `paycheck_type`, cadence, surplus-routing as first-class fields) so NO future migration
is needed if his pay changes — but BUILD only the mode he's on now (semimonthly, fixed). Stub the
bi-weekly buffer + hourly paths until his pay actually changes. **Design-complete, build-what-you-use**
(guards the 99.99%-tail / over-build reflex). A correctly extensible model IS "being prepared."

### 4.9 The buffer, deliberate surplus & large purchases (V1 = Zach's personal model)
**Clarified 2026-06-07: buffer and "emergency savings" are ONE pool, and the real axis is NOT
emergency-vs-planned — it's COMPULSORY-REPAYMENT vs PRE-SAVED.** "Emergency" is a misnomer here; it
means **any large expense that can't be recouped in a single month** (a $2,000 MacBook or an $8,000
surgery alike), versus a small one-month deficit (a $200 overspend) which the rolling Other bucket
(§4.3) absorbs next month. That recoup-horizon is the line between "Other handles it" and "this is a
buffer/large-purchase event."

Two mechanisms for large expenses (mirror images around the purchase date):
- **Buffer draw — "pay off in X months" (borrow path; COMPULSORY repayment).** The full price is
  marked **spent immediately** (accurate tracking — you have the laptop / had the surgery), the cash is
  fronted by the **buffer** (a loan to yourself), and the budget absorbs it via **compulsory** monthly
  repayment installments that flow back INTO the buffer, shown as "X of Y paid," until the buffer is
  restored. Repayment is non-optional — that's the defining trait of the buffer.
- **Surplus draw — "pay through surplus" (pre-saved path; NO repayment).** A deliberate surplus saved
  AHEAD into its own fund; the purchase **draws the fund down** with no repayment (already funded). The
  draw is a fund-draw, **not a re-charged budget expense** (reuses §4.5 no-double-charge + the
  sinking-fund payout logic). The deliberate surplus contribution does NOT count as monthly income — it
  is an expense out of spendable budget into the fund.

**Funds = one primitive, kinds (see also sinking funds §4.7):**
- **`buffer`** (= "emergency" = Zach's single savings pool): tappable, **`compulsory_repayment=true`**,
  has a lean **`target_balance`**. Drawing creates a repayment obligation; repayment restores it to
  target, not beyond.
- **`surplus`**: tappable, `compulsory_repayment=false`, saved toward a specific planned purchase.
- (`sinking`, §4.7: auto-accrues toward scheduled recurring bills — same fund family.)

**Kept LEAN on purpose:** Zach dislikes idle cash — anything ABOVE the buffer's `target_balance` he'd
rather have in the market. So repayment restores TO target, and the app should **flag** when the buffer
is above target (excess to invest, externally) or below target with outstanding obligations (don't
stack another large draw). This is a **judgment AID, not enforcement** — he's financially literate
enough to know a $2,000 purchase is fine normally but unwise right after an $8,000 surgery; the app
surfaces buffer health + outstanding repayment obligations so his judgment has the data. It does NOT
block.

**V1 scope:** this encodes **Zach's personal operating model**, built **totally tailored to him** (sole
user). The compulsory-repay-buffer philosophy "wouldn't work for everyone," and he's open to making it
customizable later — but V1 is how he has always operated, and it's never given him trouble. (Consistent
with the buffer-dependent features being opt-in by financial situation, §4.8.) For V1 the **only active
buffer is this emergency/working savings pool**; the income-smoothing buffer (§4.8) stays dormant
because Zach is semimonthly (always 2 paychecks/month, so nothing to smooth).

### 4.10 Expected expenses (manual placeholders for known-but-not-yet-charged costs)
A real behavioral need (added 2026-06-07): Zach commits to deferred charges (e.g. books an AirBnB whose
payment is deferred a month) and forgets to account for them, then gets surprised when they land. An
**expected expense** is a **manually-entered placeholder** — amount, category, description, and a
**target month** (the month it's expected to actually charge) — that **RESERVES budget** (counts in
budget math) in that target month BEFORE the real charge appears, so the month isn't overspent when the
charge pops up.

- **Opposite budget treatment from Plaid `pending` (§4.4) — the key distinction.** Plaid pending = a
  charge Plaid sees but hasn't settled; **EXCLUDED** from budget math (uncertain / transient). An
  expected expense = a commitment the USER knows is coming; **INCLUDED** (the whole point is to reserve
  the budget). Different `status`, opposite handling.
- **Forward-datable:** the target month can be a FUTURE month (book in June, reserve July's budget), so
  a known future month already accounts for the commitment.
- **Settle-on-match (reuses §4.5 no-double-charge + the flexible_set settle pattern):** when the real
  charge arrives (Plaid or manual), the user **matches** it to the placeholder; the placeholder is then
  **replaced/settled by the real transaction** — never both, so no double-count. Same "placeholder until
  a real charge replaces it" mechanic as flexible_set (§4.2), applied to a one-off manual commitment
  instead of a recurring category.
- **Stale handling at month-close:** an expected expense still unmatched when its target month closes
  should **prompt** (carry forward if still expected, or remove to release the reserved budget) — never
  silently drop it or silently overspend.
- Modeled as a `transactions` row with `status='expected'`, `source='manual'`.

## 5. Data model / schema

```
users
  id, email, password_hash, totp_secret?,
  tracking_start_date,        -- day-1 / genesis cutover (D8, §12); everything before is CLOSED; Plaid never ingests pre-this-date txns
  created_at

webauthn_credentials          -- §9.1: passkeys / biometric login (WebAuthn); one user, many devices
  id, user_id→users,
  credential_id UNIQUE, public_key, sign_count,
  transports?, aaguid?, nickname?, created_at, last_used_at?
  -- session storage is Postgres-backed (server-side session store manages its own table); not modeled here

budgets                       -- versioned config; months REFERENCE this
  id, user_id→users, name, effective_from, effective_to?(null=current), created_at

categories                    -- buckets, belong to a budget version
  id, budget_id→budgets,
  category_key,               -- stable lineage id across budget versions (D3); cross-version reporting DEFERRED, column added now
  name, amount,
  grp ('fixed' | 'discretionary'),
  settle_type ('true_set' | 'flexible_set' | null),
  expected_bills int?,        -- flexible_set only
  is_rollover_bucket bool,    -- exactly one true PER budget version; enforce with a DB partial unique index on (budget_id) WHERE is_rollover_bucket (D #11, §12)
  cadence ('monthly'|'quarterly'|'semiannual'|'annual') default 'monthly',  -- >monthly = sinking fund (§4.7)
  period_months int?,         -- arbitrary cadence override; null = use cadence enum
  fund_balance numeric default 0,  -- sinking-fund carryover (virtual envelope, §4.7)
  next_due_date date?,        -- sinking-fund next occurrence = accrual anchor
  sort_order
  -- (income setup lives in `paycheck_config`, §4.8; income flows reuse `transactions` w/ income_kind)

accounts
  id, user_id→users, name, type ('checking'|'credit'|...),
  plaid_account_id?, plaid_item_id→plaid_items?

plaid_items                   -- one per linked institution
  id, user_id→users, institution_name,
  access_token_ref,           -- Key Vault secret REFERENCE, never the raw token
  sync_cursor?,               -- Plaid cursor for incremental pulls
  last_synced_at?, created_at

months
  id, user_id→users, budget_id→budgets,    -- reference, not copy
  year, month, status ('open'|'closed'),
  opened_at, closed_at?,  UNIQUE(user_id, year, month)
  -- month-membership computed in the fixed home TZ America/New_York; all timestamps stored UTC (D2, §12)

transactions
  id, user_id→users, month_id→months,
  category_id→categories?,    -- null = uncategorized (just pulled)
  account_id→accounts?,
  date, amount,               -- signed; negative = expense
  description, source ('manual'|'plaid'),
  plaid_transaction_id? UNIQUE,   -- dedup
  status ('pending'|'settled'|'expected'),  -- 'expected'=manual placeholder, COUNTS in budget (§4.10);
                                            -- 'pending'=Plaid-seen-unsettled, EXCLUDED (§4.4)
  is_rollover bool,           -- system-generated 1st-of-month line item
  created_at, updated_at

funds                         -- §4.9: buffer/emergency (compulsory-repay) + deliberate surplus (pre-saved)
  id, user_id→users, name,
  kind ('buffer' | 'surplus'),
  balance numeric default 0,
  target_balance numeric?,    -- buffer only: lean target; excess → market (external)
  compulsory_repayment bool,  -- true = buffer, false = surplus
  created_at

repayment_obligations         -- created when the buffer funds a large purchase ("pay off in X months")
  id, user_id→users,
  fund_id→funds,              -- the buffer being repaid
  transaction_id→transactions,-- the large purchase (marked spent in full at purchase)
  total_amount, remaining_amount,
  installment_amount, months_remaining,
  status ('active' | 'paid'), created_at
  -- repayment installments = compulsory monthly budget expenses flowing back into the buffer until paid
  -- a large-purchase txn settles as: pay_in_full | pay_through_surplus(fund_id) | buffer_financed(→obligation)
```

Computed (query/materialize, not stored):
- category actual-spent (per month) = `settled ? sum(settled txns in category) : placeholder`
- category remaining = `budget_amount - actual_spent`
- month net leftover = `(actual income − expected income) + sum(remaining across categories)` → becomes next month's rollover txn (income variance nets into Other by formula, not a discrete line item; D5 = B, §12)
- fund contributions (sinking accrual / surplus / buffer repayment) are an **expense against the month** and are **excluded** from the net-leftover above, so an earmarked dollar is counted once (D6 / `BUDGET-FUND-EARMARK-1`, §12)

**Multi-user-shaped, single-user-built:** every core table has `user_id` (free future-proofing) but
DO NOT build any multi-user features (see §9).

## 6. Plaid integration (bank auto-pull)

- BoA has **no public consumer API.** Use **Plaid** (aggregator). Embedded in-app via **Plaid Link**
  (frontend widget). The "pull" button opens Plaid Link → user does BoA's OAuth login INSIDE the
  secure widget → app never sees the BoA password → returns a short-lived `public_token` → backend
  exchanges for an `access_token` (store as a Key Vault secret reference, never raw in DB).
- **Read-only scoping:** request ONLY the **Transactions** product (+ Accounts for naming). Money
  movement is a SEPARATE product (Transfer) that you simply don't enable → the token *physically
  cannot* move money. This is exactly Zach's security ask. Residual exposure: read-only ≠ private —
  Plaid + the app CAN read full transaction history; guard the token like any secret.
- **Incremental sync via `/transactions/sync` (cursor-based), NOT date-range:** store `sync_cursor`
  per plaid_item; each sync returns `added / modified / removed` with stable `transaction_id`s. This
  single mechanism handles BOTH dedup (stable IDs + cursor; no need to track last-pull datetime
  manually) AND the **pending→settled transition** (settled versions arrive via `modified`, linked by
  `pending_transaction_id`).
- **Rolling 30-day reconcile on every pull (DECIDED 2026-06-07):** beyond the cursor sync (which
  already returns `modified`), each pull explicitly **re-reconciles the trailing 30 days** of
  transactions against what Plaid currently reports, catching any amount/category/pending changes to
  recent charges (modifications cluster within days of the original charge, so a rolling 30-day window
  covers realistic drift while bounding the work to a fixed window, not all-history). AFTER reconciling
  the prior 30 days, the genuinely NEW (`added`) transactions land uncategorized for the user to assign.
- Pull flow: pulled transactions land `category_id=null` (uncategorized); user assigns each to a
  category in the chorale table (§7); assigning to a fixed category settles it against the placeholder
  (§4.5).
- **Sign convention + removals (RESOLVED 2026-06-07, §12):** Plaid reports positive = outflow; the
  internal convention is signed `negative = expense`. Normalize (flip) **once, at the Plaid mapper
  boundary** (`BUDGET-PLAID-SIGN-1`), with a direction test; no downstream code re-interprets Plaid
  sign. A `removed` transaction that had already settled a fixed category or matched an expected
  expense **reverses** that settlement (restore the placeholder / un-match) (`BUDGET-SETTLE-ON-MATCH-1`).
- **Genesis cutover guard (RESOLVED 2026-06-07, §12 D8):** the first cursor sync and the rolling
  30-day reconcile **exclude any transaction dated before `users.tracking_start_date`** — the pre-day-1
  world is already captured in the onboarding opening snapshot (§4.6), so ingesting those rows would
  double-count. The 30-day reconcile window is clamped to `max(today − 30 days, tracking_start_date)`
  (`BUDGET-CUTOVER-1`).
- Plaid cost: free dev tier; single-user/single-bank is cheap/free. Production access needs a (small)
  Plaid account approval.

## 7. chorale usage (the UI)

The main screen is a **transactions table** built on chorale (Zach's own library — dogfood):
- **Grouping + aggregation (chorale v0.2.0 Item 8):** group transactions by day (and/or category),
  show per-category aggregate amounts, expand a group to reveal the individual expenses. This is the
  "select the day → see the specific expenses" drill-down Zach wants.
- **In-cell editing:** assign/change a transaction's category inline (dropdown), edit amount/name.
- **Filtering / sorting:** by date, account, category, settled/pending.
- Likely client-rendered or SSR Dioxus (the portfolio site work will establish the Dioxus deploy
  pattern; reuse it here).
- **Dogfood sequencing (DECIDED 2026-06-07):** do NOT gate chorale v0.2.0's release on this app. Ship
  chorale on its own QA gate; this app becomes chorale's **first real consumer** and dogfoods into
  chorale 0.2.1 / 0.3.0 (pre-1.0, so API changes stay cheap). OPTIONAL: a hard-timeboxed **1-day**
  pre-release slice (just the transactions grid) as consumer-ergonomics insurance while the API is
  still free to change — drop it if it threatens to balloon.
- **Consuming chorale BEFORE it's published (answered 2026-06-07): yes, standard Cargo practice.**
  During dev, depend on it via a **git dependency on `main`** (RESOLVED 2026-06-07, Zach's directive):
  `chorale-dioxus = { git = "https://github.com/zernst3/rust-chorale", branch = "main" }`. Once v0.2.0
  is published to crates.io, swap to `chorale-dioxus = "0.2.0"`. API churn during the pre-release window
  is EXPECTED and is the point — building against it IS the dogfood that hardens the final API before
  it's frozen. (Verify `main` carries the v0.2.0 grouping + in-cell-editing surface the transactions UI
  needs; if that work is still on a draft-release branch at build time, point the dep there instead.)

## 8. Tech stack & infra

- **Backend:** Rust monolith (Axum + SeaORM, mirroring the Agora-rs port patterns). **API boundary =
  Dioxus server functions calling services directly (D1, §12); the services + repositories layers are
  unchanged from the Agora pattern; no hand-written REST controllers. Agora keeps its REST controllers
  and is not affected.**
- **DB:** PostgreSQL.
- **Frontend:** Dioxus + chorale.
- **Bank:** Plaid (Transactions product only).
- **Cloud:** Azure (Zach's choice — familiar, environmentally sound; see the broader cloud decision
  notes — DigitalOcean was rejected, Azure-simplify chosen). Run it himself via **Terraform**
  (basically the Agora infra pattern shrunk):
  - Azure Container Apps (scale-to-zero) for the monolith
  - **DB: Neon free-tier Postgres (RECOMMENDED, revised 2026-06-07)** — serverless, scale-to-zero,
    $0 for single-user/tiny data; has Azure regions so the cross-vendor hop is negligible. Terraform
    still manages BOTH (Azure + Neon providers in one config), preserving "one source of truth."
    *Fallback if all-in-one-vendor is preferred:* Azure Database for PostgreSQL Flexible Server,
    Burstable B1ms — but that's a **~$15-18/mo floor** (Azure has no serverless/scale-to-zero Postgres),
    i.e. ~$200/yr for marginal single-vendor simplicity on a one-user app. Not worth it; go Neon.
  - Azure Key Vault (Plaid token + DB creds)
  - GitHub Container Registry for the image (skip ACR's ~$5/mo flat tier)
  - (Watch the Log Analytics workspace: 5 GB/mo free tier — stay under it with non-verbose logging.)
- **Estimated cost: ~$0-5/month** on the recommended path (Neon free + scale-to-zero compute + GHCR);
  ~$20-23/mo only if Postgres is kept in Azure (B1ms ~$15-18 + ~$5 incidental). "I'm not THAT cheap"
  → fine to leave ACR at ~$5 if preferred, but the DB is where the real money is, and Neon makes it $0.

## 9. Auth & scope (DECIDED)

- **Single-user. Build real auth so only Zach can log in** (financial data + a Plaid token = treat
  security seriously; reuse Agora's Key Vault + TOTP patterns).
- **Do NOT build for other users.** Multi-user financial-data SaaS triggers Plaid production vetting,
  data-protection compliance, breach liability, and a real security posture — a different *project*
  with legal exposure, not worth it for a personal tool.
- The schema is already multi-user-shaped (`user_id` everywhere) so it doesn't preclude a future
  product, but write **zero** multi-user code now.

### 9.1 App-level security — how "only Zach sees the data" (RESOLVED 2026-06-07)
The BoA/Plaid layer secures the *pull* (read-only token in Key Vault, §6). This subsection secures the
*app* and is encoded as `BUDGET-AUTH-GATE-1`:
- **No public signup.** The single user is provisioned out of band (seed/CLI); the site exposes only a login.
- **Login = password (Argon2) + mandatory TOTP**, plus **passkeys / WebAuthn** for biometric login
  (Touch ID / Face ID / fingerprint). Passkeys are day-to-day; TOTP is the fallback. Reuses Agora's auth
  patterns. (A web app cannot read the fingerprint sensor directly; WebAuthn has the OS mediate the
  biometric and return a public-key assertion.)
- **Sessions = secure, HttpOnly, SameSite=Strict cookies**, backed by a **Postgres-backed server-side
  session store** so sessions survive the scale-to-zero cold starts (the store manages its own table).
- **Enforce-by-construction authz:** every data-returning server function / route requires an
  `AuthedUser` extractor; without a valid session it returns 401 and reaches no data. Request identity is
  obtainable ONLY through that extractor, so an ungated data path cannot return data by construction.
- **Every query is scoped to the authenticated `user_id`** (defense in depth).
- **HTTPS-only ingress** (Container Apps managed TLS; insecure connections disabled).

## 10. Open questions / to revisit when building

> **All Phase-1 open questions are now RESOLVED (2026-06-07); see §12 for the authoritative resolutions. The items below are retained for historical context.**

- Exact rollover math edge cases: how the flexible-set "pending placeholder" interacts with the
  month-close net-leftover computation (don't double-count a placeholder that hasn't settled).
- Whether to show the conservative in-month "Other" live view (§4.3) or just the rolling balance.
- Account model granularity (one BoA checking + one BoA credit card to start).
- Historical import: **DECIDED — do NOT backfill.** See §4.6 initial-load: per-category summary opening
  charges as-of-today + a correct starting balance is all the rolling balance needs (xlsx at
  `~/Downloads/Budget.xlsx` is reference only, for the starting numbers).
- Category reordering / archiving (categories change over budget versions).

## 11. Build notes for the background routine

- Front-load all decisions above; they're settled. Escalate only genuinely new one-way-doors.
- Lean on the Camerata Rust rule library + Agora-rs conventions (CONVENTIONS.md patterns: layered
  Controllers→Services→Repositories, REPO-7 intra-aggregate tx via begin/commit, REPO-10 UoW at
  service for cross-aggregate, etc.) — most architectural decisions are already captured there, which
  is the whole point of the methodology and should make this efficient.
- Source xlsx for reference: `/Users/zacharyernst/Downloads/Budget.xlsx` (formulas reverse-engineered
  above; the live logic was in the 'June 2026' / 'Blank 2026' tabs).
- **Intended build method (2026-06-07): an autonomous-routine TEST of the orchestration setup.** Feed
  this (now-comprehensive) SPEC as the precise requirements → a routine maps them against the Camerata
  rules + suggests project-specific rules + investigates + emits a report for review → after review, a
  tiered routine completes the work, model-tiered to the task, capped only at a budget cap. **Honest
  expectation (this week's checker-dependence lesson):** the backend/domain (Rust → compiler + tests +
  clippy = a real deterministic checker) will autonomously progress well; the **chorale UI integration
  is the checker-poorer half** where manual QA + staying in the loop is required (same boundary as
  chorale itself). The precise spec + checker-rich verification + the governance (budget cap, pause
  flags, tiering, auto-call escalation) are what make this a fair test rather than a flail.

---

## 12. Phase-1 Resolved Decisions (AUTHORITATIVE — folded in 2026-06-07)

All Phase-1 open decisions from `PHASE_1_REPORT.md` are resolved. This section is authoritative and
supersedes any earlier ambiguity in §§1–11. The eleven project-local rules referenced here are defined
in `CONVENTIONS.md` (the Camerata-emitted ruleset).

### Blocking decisions
- **D1 — API boundary: Dioxus server functions → services → repositories.** Server functions are the
  thin entry layer (replacing hand-written REST controllers + client fetch), calling services directly
  (`RUST-DIOXUS-9`). The service and repository layers and crate boundaries (report §4) are unchanged.
  Agora keeps its REST controllers and is **not** touched.
- **D4 — Unsettled flexible_set placeholders at month-close: count the placeholder.** Every category
  contributes `settled ? sum(settled txns) : placeholder` to the net (`BUDGET-NO-DOUBLE-CHARGE-1`
  applied at close). When the real bill settles later, the variance (actual − placeholder) reconciles
  through the rolling chain in the month it **settles** in. The small timing imprecision is accepted.
- **D5 — Income variance nets into Other, by formula (option B).**
  `net = (actual income − expected income) + Σ(expense category remaining)`. A higher-than-expected
  paycheck raises Other by the surplus by formula (no discrete line item), matching the 4-year sheet.
  Counted **once**: surplus-routing + the smoothing buffer are smoothed-mode-only, and Zach is
  per-paycheck (no competing buffer). The wellness-reimbursement "add to this month" checkbox is a
  smoothed-mode override only; in per-paycheck mode the surplus auto-raises Other with no checkbox.
- **D6 — Fund contributions are month expenses (`BUDGET-FUND-EARMARK-1`): CONFIRMED.** Money into any
  fund (sinking accrual, surplus contribution, buffer repayment) is an expense against that month,
  reduces free-to-spend, and is **excluded** from the rollover net, so an earmarked dollar is counted
  once. Fund **draws** are fund-draws, not re-charged expenses (`BUDGET-NO-DOUBLE-CHARGE-1`).
- **D7 — Buffer-financed purchase: full price tracked, ZERO month-budget impact.** The full-price
  transaction posts for tracking but does not hit the month's budget; it is offset by the buffer draw
  (a fund-draw fronting the cash). The `repayment_obligation`'s monthly installments **are** the
  month-budget expenses, flowing back into the buffer until `remaining = 0`.

### Decide-now (schema/affordance added now, feature deferred)
- **D2 — Month-membership in a fixed home timezone `America/New_York`; timestamps stored UTC**
  (`ARCH-UTC-TIMESTAMPS-1`). Travel-stable. The TZ is a config value (see clarification below).
- **D3 — Add a `category_key` lineage column to `categories` now.** Cross-version reporting is **not**
  built in V1 (design-complete, build-what-you-use); the column is the only affordance added.

### Lower-risk (resolved; build in the relevant slice)
- **`BUDGET-PLAID-SIGN-1` — ADOPT.** Plaid positive-outflow → internal `negative = expense`, flipped
  once at the mapper boundary, with a direction test.
- **Forward-dated expected expense → eager-create the target month** (extend lazy-init to create-forward);
  `transactions.month_id` stays non-null.
- **Partial flexible_set at close → still `pending`, counts the placeholder** until all `expected_bills`
  land (then `settled` → sum). Same predicate as D4.
- **Plaid `removed`-after-settlement → reverse the settlement** (restore placeholder / un-match the
  expected expense) via `BUDGET-SETTLE-ON-MATCH-1`. Build in the Plaid slice.
- **Rollover bucket = exactly one per budget version**, enforced by a DB partial unique index on
  `(budget_id) WHERE is_rollover_bucket`.

### Onboarding cutover / "day 1" genesis (D8 — added 2026-06-07)
There is an explicit **`tracking_start_date` (day 1)**: a chosen cutover date that is the genesis of
tracking. Everything before it is **closed** and represented solely by the onboarding opening snapshot
(per-category month-to-date summary opening charges + the correct starting Other/buffer balances),
taken as of the end of day 0 so any spend in the **lock gap** between the last spreadsheet day and day
1 is included. From day 1 forward the app tracks per-transaction. **Plaid never ingests transactions
dated before `tracking_start_date`** (the pre-genesis world is already in the opening snapshot;
ingesting it would double-count). Day 1 may be any date; a month-start day-1 gives the cleanest first
month, but a mid-month day-1 is fully handled by the month-to-date opening snapshot. Encoded as
`BUDGET-CUTOVER-1` and the `users.tracking_start_date` column.

**Zach's intended onboarding path (2026-06-07):** run a live **test phase** (real Plaid) until the 1st
of a month, then do a **clean month-start cutover** — set `tracking_start_date` to the 1st, provide
only the **prior month's surplus** as the starting Other balance, and track everything else normally
(per-category test "expenses" entered during the test phase; utilities at $0 settle normally). This is
the cleanest case and eliminates the partial-first-month and lock-gap complications. Onboarding must
therefore be **re-runnable** (a test phase, then a clean reset to the real day 1), and a month-start
day-1 needs only the starting balances, not per-category month-to-date charges.

### App-level security (D-sec — added 2026-06-07)
"Only Zach can see the data" is a layer separate from the BoA/Plaid read-only pull. Encoded as
`BUDGET-AUTH-GATE-1`, detailed in §9.1: no public signup; password (Argon2) + mandatory TOTP +
passkeys/WebAuthn (biometric login); secure HttpOnly SameSite cookies on a **Postgres-backed** session
store (survives scale-to-zero); an `AuthedUser` enforce-by-construction gate on every data path; all
queries scoped to the authenticated `user_id`; HTTPS-only. Schema adds a `webauthn_credentials` table
(§5, design-complete). Built in the auth slice (report §8 step 7).

### PWA (mobile) — frontend phase
The app ships as an **installable PWA** (web app manifest + a thin service worker + icons; standalone
display) so it installs to the phone home screen. **Not** offline-first (a server-backed budget app
needs the DB; offline sync is unjustified complexity per `SPIRIT-ROBUSTNESS-1`). Built in the frontend
phase alongside the chorale UI; biometric login uses the passkeys above.

### Project-local rules adopted (full definitions in `CONVENTIONS.md`)
`BUDGET-MONEY-1`, `BUDGET-NO-DOUBLE-CHARGE-1`, `BUDGET-ROLLOVER-INTEGRITY-1`,
`BUDGET-PLAID-TOKEN-VAULT-1`, `BUDGET-STATUS-DRIVES-INCLUSION-1`, `BUDGET-FUND-EARMARK-1`,
`BUDGET-IDEMPOTENT-MONTH-INIT-1`, `BUDGET-PLAID-SIGN-1`, `BUDGET-SETTLE-ON-MATCH-1`,
`BUDGET-CUTOVER-1`, and `BUDGET-AUTH-GATE-1` (11 total). `BUDGET-MONEY-1` is an elevation to
*mechanical* enforcement of the existing `ARCH-EXACT-DECIMALS-1` / `DOMAIN-8`. The full project ruleset
(stack-applicable Camerata library rules + these 11) is emitted to `CONVENTIONS.md` (structured/
mechanical) and `AGENTS.md` (prose) in Camerata format.

### Phase-2 build setup (resolved 2026-06-07)
- **Repo:** a new **public** git repo at **`~/Documents/Repos/budget-tracker`** (local + GitHub
  `zernst3/budget-tracker`), created and pushed by the routine. Not under `New Projects/`.
- **chorale dependency:** git dependency on **`main`** of `github.com/zernst3/rust-chorale`; swap to the
  `chorale-dioxus = "0.2.0"` crate once published (§7).
- **Routine output = code + Terraform ONLY.** The routine writes all application code, the Terraform,
  and the CI/CD workflow. It does **NOT** deploy and does **NOT** set secrets. Zach provisions the
  Plaid + Neon (+ Azure) accounts, sets the GitHub Actions secrets/variables, and runs the first
  deploy, walking through that step interactively with Claude. CI is expected to be red until those
  secrets exist. None of these accounts are needed to *develop* the code + Terraform, so the build is
  unblocked without them.
- **Trigger model:** **fully manual.** No nightly schedule; Zach triggers each run explicitly.
  **Budget cap = $20 per run.**
- **Scope:** full — **backend and frontend** (report §8 steps 1–10, including the chorale UI). The
  chorale UI slice is the checker-poor half (§11) and is where a run is most likely to need Zach's
  manual QA between triggers.
- **Build routine:** **multi-model / model-tiered** — cheap models for bulk/mechanical work, top-tier
  for architecture and one-way-door calls (`ORCH-MODEL-TIERING-1`); governed by the $20/run cap, pause
  flags, and auto-call escalation.
- **Timezone:** home TZ confirmed **`America/New_York`** (D2).

### Net
All Phase-1 decisions resolved. Phase-2 sequencing (report §8) can proceed once the prerequisites in the
build-handoff (external accounts/credentials, repo name, home-TZ confirmation) are in place.

