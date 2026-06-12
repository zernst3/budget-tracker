# Stage-1 Local Testing Runbook

Run the full budget-tracker app on your laptop — no real Plaid, no Neon, no
Azure. The mock Plaid integration serves the same fixture pages through the
real mapper and sync engine, so Pull -> Pending -> triage, the month ledger,
and fund math can all be shaken out before a deploy.

---

## Prerequisites

- Rust toolchain (see `rust-toolchain.toml`)
- Docker (for local Postgres)
- `dx` CLI: `cargo install dioxus-cli` (same version as `dioxus-cli-config` in
  `Cargo.toml`)
- A TOTP authenticator app (Authy, 1Password, Google Authenticator, etc.)

---

## Step 1 — Start a local Postgres

```bash
docker run -d \
  --name budget-local-pg \
  -e POSTGRES_USER=budget \
  -e POSTGRES_PASSWORD=localpass \
  -e POSTGRES_DB=budget_local \
  -p 5432:5432 \
  postgres:16
```

Set the connection URL for every subsequent command:

```bash
export DATABASE_URL="postgres://budget:localpass@localhost:5432/budget_local"
```

---

## Step 2 — Set the local environment

Copy these into your shell (or a `.env` file you source manually):

```bash
export DATABASE_URL="postgres://budget:localpass@localhost:5432/budget_local"

# Enable the mock Plaid integration (CRITICAL SAFETY: never set in production).
export PLAID_MODE=mock

# Allow the session cookie without HTTPS on localhost.
export SECURE_COOKIES=false

# WebAuthn relying-party — must match the origin you open in the browser.
# The dx serve default is port 8080.
export WEBAUTHN_RP_ID=localhost
export WEBAUTHN_RP_ORIGIN=http://localhost:8080

# Session secret (any 64+ hex chars; only used locally).
export SESSION_SECRET=0000000000000000000000000000000000000000000000000000000000000001
```

> No `PLAID_CLIENT_ID`, `PLAID_SECRET`, or `KEY_VAULT_URL` are needed.
> The server will log a loud WARN at startup confirming mock mode is active.

---

## Step 3 — Run migrations

Migrations are applied automatically by every seed bin and by the server at
startup, but you can run them manually to verify the schema:

```bash
cargo run -p budget-server --bin seed-local-demo --
# (seed-local-demo runs migrations as its first step; see Step 5)
```

Or via the provision-user bin (also runs migrations):

```bash
# Continue to Step 4 — provision-user runs migrations for you.
```

---

## Step 4 — Provision the single user

```bash
DATABASE_URL="postgres://budget:localpass@localhost:5432/budget_local" \
PROVISION_EMAIL="zach@local.dev" \
PROVISION_PASSWORD="supersecret-local-12" \
PROVISION_TRACKING_START="$(date +%Y-%m-01)" \
cargo run -p budget-server --bin provision-user
```

The command prints something like:

```
user created: 6ba7b810-...
email: zach@local.dev
tracking_start_date: 2026-06-01

Add this TOTP to your authenticator app (scan or paste the URI):
otpauth://totp/budget-tracker%3Azach%40local.dev?secret=ABC...&issuer=budget-tracker
```

**Scan the `otpauth://` URI with your authenticator app now.** It is the only
time the TOTP secret is surfaced. Passkey (biometric) registration requires a
real HTTPS origin and a physical authenticator — use password + TOTP for all
local testing.

Re-running `provision-user` with the same email resets the password and TOTP
(idempotent).

---

## Step 5 — Seed the demo data

```bash
DATABASE_URL="postgres://budget:localpass@localhost:5432/budget_local" \
SEED_EMAIL="zach@local.dev" \
cargo run -p budget-server --bin seed-local-demo
```

This creates (all idempotent):

| Row | Detail |
|-----|--------|
| Budget | "Local Demo Budget" (current month, no `effective_to`) |
| Categories | Rent $2500 (Fixed/TrueSet), Utilities $130 (Fixed/FlexibleSet×2 bills), Subscriptions $80 (Fixed/TrueSet), Groceries $600, Dining $300, Transport $150, Misc $200, Other (rollover bucket) |
| Month | Current calendar month, `open` |
| Buffer fund | Emergency Buffer — $5000 balance, $6000 target, `compulsory_repayment=true` |
| Surplus fund | Vacation Savings — $1200 balance, no target, `compulsory_repayment=false` |
| PlaidItem | "Mock Bank (local dev)" — `access_token_ref=mock-access-token-local-dev` |
| Accounts | BoA Checking (`mock-account-checking`) + BoA Credit Card (`mock-account-credit`) |
| Transactions | Rent $-2500 (categorized) + Whole Foods $-84.30 (categorized, with comment) |

The seed is re-runnable: a second run upserts to identical state.

---

## Step 6 — Start the app

```bash
dx serve --package budget-ui
```

Or, if you prefer a plain cargo run (same binary, different entry):

```bash
cargo run -p budget-server
```

The server binds to `http://localhost:8080` by default (the `dx serve` port).

You should see in the terminal:

```
WARN budget_infrastructure::plaid::mock_client: PLAID_MODE=mock — using the
     LOCAL MockPlaidApi + in-memory secret store (fake bank data; NO real
     Plaid / Key Vault). This is a local-testing path; it must NEVER be set
     in production.
INFO budget_server: budget-server listening address=0.0.0.0:8080
```

---

## Step 7 — Log in

Open `http://localhost:8080` in your browser.

1. Enter `zach@local.dev` and the password you set in `provision-user`.
2. Enter the current TOTP code from your authenticator app.

> Passkey (biometric) login requires a real HTTPS origin bound to `localhost`
> with a physical authenticator device. It will not work in this local HTTP
> setup. Password + TOTP is the correct local path.

---

## Step 8 — Manual QA loop

### Ledger (month view)

1. Navigate to the Ledger (the current month).
2. Verify the two pre-seeded transactions appear in day-rows (Rent $2500 on
   the 1st, Whole Foods $84.30 on the 1st).
3. Expand a day row — verify the category-grouped child table renders (Rent in
   Fixed/Rent, Whole Foods in Discretionary/Groceries).
4. Click the category dropdown on the Whole Foods row and change it to Dining.
   Verify the row moves to the Dining group after save.
5. Click the comment field on a row and add/edit a comment. Verify it persists
   on refresh.
6. Verify the budget-remaining math updates: Groceries should show $515.70
   remaining, Dining should show $215.70 remaining after the reassignment.

### Pull (first time — page 1)

7. Navigate to Pending / Triage.
8. Click **Pull**. The mock serves page 1: four transactions.
   - `mock-txn-0001-grocery` — Whole Foods $84.30 (settled, BoA Checking)
   - `mock-txn-0002-gas` — Chevron $42.15 (settled, BoA Checking)
   - `mock-txn-0003-refund` — Amazon Refund $-22.99 (settled inflow, BoA Checking)
   - `mock-txn-0004-restaurant-pending` — The Smith $67.50 (**pending** — must NOT
     appear in the Pending triage inbox)
9. The triage inbox should show **3 uncategorized rows** (the 3 settled ones).
   The pending restaurant must be absent (`SPEC §4.4`).
10. Triage the three rows through each of the three treatments:
    - **Categorize**: assign the grocery to Groceries and save. Verify it
      disappears from the inbox (now categorized, no longer pending-triage).
    - **Buffer draw**: assign the gas to Transport and mark it as a buffer draw
      (if the UI exposes it). Verify the Emergency Buffer balance decreases.
    - **Surplus draw**: assign the refund (inflow) to Misc. Verify it clears.
11. Verify the Ledger now shows the newly-categorized transactions in their
    respective day-rows.

### Pull (second time — page 2)

12. Click **Pull** again. The mock serves page 2:
    - **modified**: `mock-txn-0004-restaurant-pending` → now settled (pending
      `false`), with `pending_transaction_id` linking back to the original id.
      This is the pending->settled transition. The Smith $67.50 should NOW
      appear in the Pending inbox (it is settled and uncategorized).
    - **added**: Spotify $9.99 (a new subscription transaction, settled).
13. Triage The Smith and Spotify (categorize both).
14. Verify the inbox clears.

### Pull (third time — page 3, includes the D10 credit-card-payment scenario)

15. Click **Pull** a third time. The mock serves page 3:
    - **removed**: `mock-txn-0002-gas` (the Chevron). If it was categorized,
      it should disappear from the Ledger; if it had a settlement match, the
      placeholder should be restored.
    - **added**: Blue Bottle Coffee $5.75 (a new discretionary transaction).
    - **added**: `mock-txn-0007-cc-payment-checking` — "BANK OF AMERICA CREDIT
      CARD PAYMENT" $500.00 on the checking account. Plaid
      `personal_finance_category.detailed = LOAN_PAYMENTS_CREDIT_CARD_PAYMENT`.
      This is the **checking-outflow leg** of the D10 double-count scenario.
    - **added**: `mock-txn-0008-cc-payment-credit` — "PAYMENT THANK YOU"
      $-500.00 on the credit card account. Same `plaid_category`. This is the
      **card-side payment-credit leg** (an inflow that would wrongly offset
      expenses if not excluded).
16. Verify the gas row is gone from the Ledger.
17. Triage the coffee row (assign to a category, e.g. Dining).

### D10 internal-transfer triage (credit-card-payment double-count fix)

18. The triage inbox now shows the two credit-card-payment rows. Both should
    have the Transfer treatment **pre-selected** (the `suggested_transfer` flag
    is `true` because their `plaid_category = LOAN_PAYMENTS_CREDIT_CARD_PAYMENT`
    matches the `plaid_category_suggests_transfer` predicate).

    Verify the following for **each leg** (`mock-txn-0007` and `mock-txn-0008`):
    - The Transfer treatment is pre-selected in the triage UI (the suggestion).
      The user can override to a category treatment if they choose, but the
      pre-selection should be Transfer.
    - Confirm the Transfer treatment for each row. This sets `is_transfer=true`
      and removes the row from the inbox **without requiring a category**
      (`BUDGET-TRANSFER-EXCLUDE-1`).

19. After confirming both legs as Transfer, verify:
    - Both rows are **gone from the triage inbox** (removed via `is_transfer=true`,
      not via a category; `SPEC §4.11` D10).
    - Both rows are **visible in the Ledger** with distinct Transfer styling
      (tracked but excluded from budget math).
    - The Ledger's **day total for 2026-06-08** shows **$0.00** (or is absent)
      — the checking outflow ($500) and the card credit ($500) both cancel out
      of the expense column entirely, confirming neither leg leaked into the
      day total.
    - The category-spent totals and month **net leftover are unchanged** by
      the transfer rows — add up the spending categories and verify the transfer
      rows did not inflate or deflate any category's spend.
    - If you check the rolling Other balance (envelope-summary header), it
      must be the same as it was before the Pull that added the transfer rows.

    This end-to-end check proves the D10 invariant: with both checking and a
    credit card linked, the card-payment WITHDRAWAL on checking and the
    corresponding CREDIT on the card account are both excluded from budget math,
    so no double-count occurs.

### Pull (steady state)

20. Click **Pull** again. The mock serves the empty steady-state page (no
    added/modified/removed). The inbox should still be empty. The cursor
    is stable: subsequent pulls keep returning the empty page (idempotent).

### Funds

21. Navigate to the Funds view (if available in the current UI phase).
22. Verify Emergency Buffer shows balance $5000 / target $6000 (below target,
    flagged with outstanding-obligation indicator if you did a buffer draw
    in step 10).
23. Verify Vacation Savings shows $1200 with no target.

---

## Environment variable reference

| Variable | Required | Value |
|----------|----------|-------|
| `DATABASE_URL` | Yes | `postgres://budget:localpass@localhost:5432/budget_local` |
| `PLAID_MODE` | Yes (mock) | `mock` |
| `SECURE_COOKIES` | Yes (local) | `false` |
| `WEBAUTHN_RP_ID` | Optional | `localhost` (default) |
| `WEBAUTHN_RP_ORIGIN` | Optional | `http://localhost:8080` (default) |
| `SESSION_SECRET` | Recommended | any 64+ hex chars |
| `AI_MODE` | Yes (portfolio, offline) | `mock` — mock advisor + market data + dividend source + in-memory vault (zero network, no keys) |
| `BUDGET_USER_EMAIL` | Yes (portfolio) | the provisioned user's email (e.g. `zach@local.dev`); without it the `/portfolio` routes are not mounted |

Do NOT set `PLAID_CLIENT_ID`, `PLAID_SECRET`, or `KEY_VAULT_URL` for local
testing. If any of those are present AND `PLAID_MODE=mock` is also set, the
mock wins (explicit opt-in takes precedence). If `PLAID_MODE` is unset and
the Plaid/Vault vars are also missing, Pull returns `503` (expected: you just
have no bank linked and no mock active).

---

## Reset / re-seed

To wipe and start fresh:

```bash
docker exec -it budget-local-pg psql -U budget -c "DROP DATABASE budget_local;"
docker exec -it budget-local-pg psql -U budget -c "CREATE DATABASE budget_local;"
# Then repeat Steps 3-6.
```

Or just re-run the seed bins — they are idempotent and will upsert to the
same state without wiping anything.

---

## Troubleshooting

**"no user with email ... run provision-user first"**
Run Step 4 before Step 5.

**Pull button returns 503**
`PLAID_MODE=mock` is not set. Add it to your environment and restart the
server.

**Login fails with "invalid TOTP"**
The TOTP clock may be drifted or you scanned the wrong URI. Re-run
`provision-user` (same email) to re-enroll a fresh TOTP secret, then scan
the new URI.

**Session cookie not set / immediate redirect to login**
`SECURE_COOKIES` is not set to `false`. The browser refuses a `Secure` cookie
over plain HTTP. Set `SECURE_COOKIES=false`.

**"plaid item not found" or Pull does nothing**
`seed-local-demo` was not run, or was run against a different `DATABASE_URL`.
Verify the PlaidItem row exists:
```bash
psql "$DATABASE_URL" -c "SELECT id, institution_name FROM plaid_items;"
```

---

# Stage 2 — Portfolio Insights + DRIP (offline, `AI_MODE=mock`)

Exercise the AI Portfolio Review + real-time/DRIP tracking with **zero network
and no API keys**. `AI_MODE=mock` swaps in the mock advisor, mock market data,
and mock dividend source, all behind the same ports the real Gemini/Finnhub/
Tiingo adapters use — so the firewall, reconciliation, snapshot assembly, DRIP
accretion, and per-account upsert are all shaken out before any live key.

## Step P1 — Add the portfolio env vars

On top of the Stage-1 environment (Postgres up, user provisioned), add:

```bash
export AI_MODE=mock                     # mock advisor + market + dividends + in-memory vault
export BUDGET_USER_EMAIL=zach@local.dev # MUST match the provision-user email, or /portfolio won't mount
```

> Do NOT set `KEY_VAULT_URL` / `GEMINI_MODEL_IDS` for offline testing — `AI_MODE=mock`
> takes precedence and logs a loud WARN at startup. (Those are only for the
> live smoke test in Stage 3.)

Restart the server (`dx serve --package budget-ui` or `cargo run -p budget-server`),
log in (Stage-1 Step 7), and open `http://localhost:8080/portfolio`.

## Step P2 — Per-account upload (the upsert is the source of truth)

1. Upload positions for **one account** (e.g. account label `Brokerage`):
   `AAPL, 30` and `VOO, 10` (ticker, shares; optional cost basis).
2. The positions table renders **grouped by account**, each row showing
   shares × the mock live price = market value, plus buffer + net worth.
3. Upload a **second account** (`Roth`: `VTI, 25`). **Verify the Brokerage rows
   are untouched** — a per-account upload never disturbs another account.
4. Re-upload `Brokerage` with `AAPL, 30` only (drop VOO). Verify VOO is removed
   from Brokerage **and Roth is still intact**. This is the §2.7 per-account
   upsert: it reconciles only the uploaded account.

## Step P3 — DRIP toggle + accretion (idempotent)

5. Toggle the **DRIP checkbox** on a row (e.g. VTI in Roth). It defaults **off**.
6. Reload `/portfolio`. The mock dividend source feeds fixture dividends, so a
   DRIP-on position should show a slightly higher **estimated** share count with
   an **"estimated · N dividends reinvested since last upload"** badge (never
   shown as a confirmed figure — `BUDGET-AI-1`).
7. **Reload again** — the share count must NOT keep climbing. DRIP applies each
   dividend exactly once (idempotent catch-up). This is the key thing to eyeball.
8. Re-upload that account → the estimate resets to the uploaded baseline, **but
   the DRIP checkbox stays on** (per-position config persists across uploads).
9. Toggle DRIP **off** on a position and confirm a fixture dividend lands as
   investment-account **cash** instead of new shares (net worth still moves; the
   budget is never touched — `BUDGET-CASH-1`).

## Step P4 — Run Review (the firewall)

10. Click **Run Review**. The mock advisor (default "Verified" mode) returns
    grounded recommendations; each insight card shows its title, rationale,
    a **confidence** badge, the numbers it cites, and **validation badges**
    (Verified / Unverified) per claim, with the standing disclaimer always shown.
11. Every claim is reconciled against your real (mock) snapshot — confirm no
    number is presented as fact without a Verified badge. (The deliberately-
    hallucinated path is covered by the `advisor_mock` integration test; the
    default local mock shows the happy path.)

## Stage 3 — Live smoke test (your keys, real network) — when ready

Unset `AI_MODE=mock` and provide, in the vault / config: `KEY_VAULT_URL`,
secrets `gemini-api-key` (+ optional `finnhub-api-key`, `tiingo` key), and
`GEMINI_MODEL_IDS` (defaults to `gemini-2.5-pro,gemini-2.5-flash`). Then a real
Run Review calls Gemini, and prices/dividends resolve through Finnhub→Stooq /
Tiingo→Yahoo. **Heads-up:** the first live call is where the Gemini wire shape
(JSON-in-text and `usageMetadata` placement) gets confirmed — if it mis-parses,
it surfaces safely as `MalformedOutput` (audited, never a crash) and is a ~1-line
`wire.rs` fix. Flag it and it gets patched.
