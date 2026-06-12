# DRIP & Real-Time Position Tracking — Design (Phase 7)

Status: **DRAFT — pending Zach sign-off.** This is a data-model + new-external-port change, i.e. a one-way door (`ORCH-ONE-WAY-DOOR-1`), so it locks at sign-off before any build, exactly as the core AI Portfolio Insights feature did.

Provenance of decisions: Zach, 2026-06-12 (the DRIP conversation). Dividend-source feasibility verified by web check the same day (see §7).

---

## 0. Purpose — the loop

Track investments in **real time** so the only time the user uploads is when they **deposit** (buy shares the app cannot infer).

- **Upload = source of truth.** An upload re-baselines a position's confirmed share count and stamps a baseline date. It wipes the estimation slate for that position.
- **Between uploads, the app estimates continuously:** `current_value = (baseline_shares + DRIP accretion since baseline) × live price`, where the live price comes from the **already-built** market-data chain (Finnhub → Stooq → manual → None).
- **DRIP on (per position, per account):** when a dividend pays, the dividend is reinvested → the share count creeps up (30 → 30.78), as a **labeled estimate**.
- **DRIP off:** that same dividend becomes **cash inside the investment account** (raises net worth; never touches budget math).
- Accretion is **idempotent per dividend** (reopening the app the same day is a no-op), **persisted**, and **re-baselined on upload**. A dividend whose pay-date is on/before the upload date is **suppressed** (assumed already in the upload).

## 1. Core principles — this feature is mostly pattern-reuse

| Concern | Reused pattern | How it applies here |
|---|---|---|
| Don't store a mutable scalar; store an auditable chain | `BUDGET-ROLLOVER-INTEGRITY-1` | Current shares = `baseline_shares + Σ(drip_applications)`, always recomputable, never a mutated number. |
| Idempotent, race-safe, lazy catch-up on open | `BUDGET-IDEMPOTENT-MONTH-INIT-1` | On app open / snapshot assembly, apply each unprocessed `(position, dividend pay-date)` exactly once, in date order, via a uniqueness guard. |
| A re-baseline boundary; the past is a settled snapshot | `BUDGET-CUTOVER-1` | An upload is a new `baseline_as_of` for that position; DRIP applies only to dividends with `pay_date > baseline_as_of`. |
| Estimates are labeled, never shown as confirmed truth | `PriceProvenance` + `BUDGET-AI-1` | A position's share count carries a provenance: `Uploaded` (confirmed) vs `DripEstimated` (baseline + N events). The UI and the AI review surface the label. |
| Exact arithmetic, no floats | `BUDGET-MONEY-1` | Money exact; `shares` is an exact `Decimal` count (already true). |
| Cash balance is a stock, never a budget input | `BUDGET-CASH-1` | DRIP-off dividend cash increases the investment-account `CashBalance` only. |
| Independent external fetches run concurrently | `ARCH-PARALLEL-INDEPENDENT-1` | Per-ticker dividend fetches fan out via `try_join_all`, like the quote fetches. |

## 2. Locked decisions (this design)

1. **Best-estimate is acceptable.** Deltas are reconciled on each re-upload; the estimation logic is refined over time toward consistency. (Zach)
2. **Conservative buffer is scoped to the ESTIMATE, not the whole position.** Baseline shares × live price is not an estimate and is never haircut. The haircut applies to the **DRIP-accreted shares only**, and accreted shares are additionally **floor-rounded**. Default `DRIP_BUFFER = 0.10` (a pinned, tunable constant); `buffer_factor = 1 - DRIP_BUFFER`. Rationale: keep the displayed figure reliably "a little under" where the uncertainty actually lives, without understating confirmed holdings.
3. **Fractional-share rounding = floor to `DRIP_SHARE_DP = 3` decimal places** (pinned constant). Floor (not round) is deliberately conservative; 3 dp matches common broker fractional-DRIP precision. Tunable.
4. **DRIP-off dividend → investment-account cash, never budget.** It increases that account's `CashBalance` (`BUDGET-CASH-1`); it does not enter budget-category math. The rare real transfer of that cash into checking is booked by the user as an ordinary identifiable budget line item (out of scope for this feature).
5. **The AI review reconciles against estimated-current shares**, with the `DripEstimated` provenance surfaced on the snapshot/DTO/UI so nothing estimated is presented as confirmed. (Zach)
6. **Dividend data source (see §7):** Finnhub dividends is a premium endpoint, so dividends use a separate free chain behind a new `DividendSource` port: **Tiingo (free EOD entitlement) → Yahoo v8 keyless → manual entry**, cached aggressively. Confirm exact tiers/limits at build (`ORCH-TRAINING-CUTOFF-1`).

## 3. The estimation math (exact decimals throughout)

For a DRIP-enabled position, process its dividend events `e` (those with `e.pay_date > baseline_as_of`) in chronological order:

```
shares_held_at(e)   = baseline_shares + Σ shares_added(eᵢ) for eᵢ.pay_date < e.pay_date   // compounds
raw_new(e)          = (e.amount_per_share × shares_held_at(e)) / e.price_used               // price on pay-date
shares_added(e)     = floor( raw_new(e) × (1 - DRIP_BUFFER), DRIP_SHARE_DP )                // conservative
current_shares      = baseline_shares + Σ shares_added(e)
market_value        = current_shares × live_price                                          // live_price = existing market chain
```

DRIP **off** (same event):
```
cash_added(e)       = e.amount_per_share × shares_held_at(e)       // exact Money, no buffer (it's real cash, not an estimate)
→ increases the position's account CashBalance
```

`e.price_used` is the live/historical price on the dividend pay-date from the market chain; if unavailable, the event is held (not applied) and surfaced for manual confirmation rather than applied against a bad price.

## 4. Data model (migration `m0008_drip_realtime`)

**`positions`** (additive, expand-only):
- `+ drip_enabled BOOLEAN NOT NULL DEFAULT false` — the per-position, per-account toggle.
- `+ baseline_as_of TIMESTAMPTZ NOT NULL` — the as-of date of the current confirmed baseline (set on upload). `shares` is the confirmed baseline (immutable between uploads); current shares are derived.

**`dividend_events`** (NEW — a ticker-keyed cache, shared across positions of the same ticker, so we fetch once):
- `id UUID PK`, `ticker TEXT NOT NULL`, `ex_date DATE NOT NULL`, `pay_date DATE NOT NULL`, `amount_per_share NUMERIC NOT NULL`, `source TEXT NOT NULL` (provenance: tiingo/yahoo/manual), `fetched_at TIMESTAMPTZ NOT NULL`.
- Unique `(ticker, pay_date)`; index on `ticker`.

**`drip_applications`** (NEW — the position-keyed auditable chain; system-log semantics, `SQL-AUDIT-COLUMNS-1`):
- `id UUID PK`, `user_id UUID NOT NULL`, `position_id UUID NOT NULL` (FK→positions, Cascade), `ticker TEXT NOT NULL`, `pay_date DATE NOT NULL`, `amount_per_share NUMERIC NOT NULL`, `price_used NUMERIC NOT NULL`, `shares_added NUMERIC NOT NULL`, `cash_added NUMERIC NOT NULL` (0 when DRIP on), `drip_on_at_apply BOOLEAN NOT NULL`, `applied_at TIMESTAMPTZ NOT NULL`.
- **Unique `(position_id, pay_date)`** — the idempotency guard (apply-once). FK index on `position_id` (`SQL-DB-INDEX-1`); index on `(user_id, applied_at)`.
- No `created_by`/`updated_at` (append-only system log).

All migrations idempotent + expand-only (`PROC-CI-MIGRATION-HYGIENE-1`, `ARCH-EXPAND-CONTRACT-1`); FK indexes in the same migration (`SQL-DB-INDEX-1`).

## 5. Ports & domain types

```rust
// budget-domain — pure, no http/wire deps.
pub struct DividendEvent { pub ticker: Ticker, pub ex_date: NaiveDate, pub pay_date: NaiveDate,
                           pub amount_per_share: Money, pub source: DividendSourceKind }

pub enum ShareProvenance {
    Uploaded,                                  // confirmed as of baseline_as_of
    DripEstimated { events_applied: u32, baseline_as_of: DateTime<Utc> },
}

#[async_trait]
pub trait DividendSource: Send + Sync {
    /// Dividend events for a ticker with pay_date strictly after `since`.
    /// Ok(vec![]) = none. # Errors DividendSourceError::{Api, RateLimited, SecretVault}.
    async fn dividends_since(&self, ticker: &Ticker, since: NaiveDate)
        -> Result<Vec<DividendEvent>, DividendSourceError>;
}
```

`PricedPosition` gains `pub share_provenance: ShareProvenance`; `PortfolioSnapshot` carries it through so the AI review and the DTO can label estimated positions. Adapters: `TiingoDividendSource`, `YahooDividendSource` (keyless), `ManualDividendSource`, `MockDividendSource`, composed by a `ChainDividendSource` (same shape as `ChainMarketDataProvider`).

## 6. The DRIP catch-up service (lazy, idempotent, on open)

A new app-services use-case, run during snapshot assembly (or app open):
1. For each position, fetch (cached) dividends since `baseline_as_of`, concurrently per distinct ticker (`try_join_all`).
2. For each `(position, pay_date)` with no existing `drip_applications` row: compute per §3, insert the application (DRIP on → `shares_added`; DRIP off → `cash_added` to the account `CashBalance`) under the `(position_id, pay_date)` unique guard with `ON CONFLICT DO NOTHING`, in chronological order.
3. Race-safe and re-entrant (two opens, or a same-day re-open, post nothing extra) — exactly the `BUDGET-IDEMPOTENT-MONTH-INIT-1` guarantee.
4. Events with `pay_date <= baseline_as_of` are skipped (upload already includes them).

**Upload (re-baseline):** sets `positions.shares = uploaded`, `baseline_as_of = upload_date`. Prior `drip_applications` are retained for audit but no longer contribute (current = baseline + applications with `pay_date > baseline_as_of`). A dividend with `pay_date == upload_date` is suppressed.

## 7. Dividend-data feasibility (verified 2026-06-12)

- **Finnhub dividends = premium**, not free (free tier = quotes/news/basic-fundamentals/filings). So dividends do **not** ride on the Finnhub key.
- **Free options that return ex-date + pay-date + amount:** **Tiingo** (free EOD entitlement, documented) — primary; **Yahoo v8** `chart?events=div` (keyless, undocumented-but-works) — fallback; **Alpha Vantage** (free, 25 req/day) — viable given dividends are quarterly and heavily cached.
- Chain: **Tiingo → Yahoo → manual entry**, cached in `dividend_events`. Manual entry is the ultimate fallback (mirrors the manual-price fallback): the app proposes, the user confirms `$/share`.
- `ORCH-TRAINING-CUTOFF-1`: confirm exact current tiers, limits, and endpoint shapes at build time; treat them as best-effort behind the port.

## 8. Firewall / AI-review interaction

`BUDGET-AI-1` still holds: the AI cannot assert any figure outside the snapshot, and every claim is tuple-reconciled. What changes: for `DripEstimated` positions the snapshot's share count is a **labeled estimate**, so the reconcile still runs but the badge/label notes "share count estimated since last upload." This keeps the honesty invariant intact — an estimate is never rendered as confirmed truth — while letting the review reflect real-time values (Decision §2.5). Re-baselining on upload restores `Uploaded` provenance.

## 9. Phases (each green-gated; P7.1 pauses for sign-off — one-way-door data model)

- **P7.1 — data model + ports.** `m0008` migration, `positions` columns, `dividend_events` + `drip_applications` tables + entities + mappers; domain `DividendEvent`/`ShareProvenance`/`DividendSource` port + error enum. **Pause for sign-off** before building on it.
- **P7.2 — dividend sources.** `MockDividendSource` + `ManualDividendSource` first; then `TiingoDividendSource` + `YahooDividendSource` + `ChainDividendSource`. Cache wiring. Mock-tested.
- **P7.3 — the catch-up engine (longest pole).** The idempotent lazy DRIP service + the §3 estimation math (buffer + floor) + DRIP-off→cash, built and tested entirely against mocks. Idempotency/re-entrancy tests; chronological-compounding tests; suppression-on-upload tests.
- **P7.4 — wire-in + UI.** Provenance into `PortfolioSnapshot`/DTO; the per-position DRIP toggle UI; the "estimated since last upload" badge; re-baseline-on-upload; the AI review reflecting estimated values with labels.
- **P7.5 (optional) — delta refinement.** On each upload, record the realized-vs-estimated delta per position to a log so the buffer/rounding can be tuned over time toward consistency (Decision §2.1).

## 10. Open items requiring confirmation (`ORCH-TRAINING-CUTOFF-1`)
1. Exact dividend source + tier (Tiingo free entitlement scope / Yahoo keyless reliability) — confirm at build.
2. `DRIP_BUFFER` default (0.10, scoped to accreted shares) and `DRIP_SHARE_DP` (3, floor) — tunable; confirm starting values.
3. Whether the per-account DRIP toggle should default off (recommended: off — DRIP is opt-in).

## 11. Conventions cited
`BUDGET-IDEMPOTENT-MONTH-INIT-1`, `BUDGET-ROLLOVER-INTEGRITY-1`, `BUDGET-CUTOVER-1`, `BUDGET-CASH-1`, `BUDGET-MONEY-1`, `BUDGET-AI-1`, `ARCH-EXPAND-CONTRACT-1`, `ARCH-PARALLEL-INDEPENDENT-1`, `RUST-DOMAIN-1/2/3/4/6/7`, `RUST-ENTITIES-1..13`, `RUST-MAPPER-1`, `SQL-AUDIT-COLUMNS-1`, `SQL-DB-INDEX-1/2`, `PROC-CI-MIGRATION-HYGIENE-1`, `ORCH-TRAINING-CUTOFF-1`, `ORCH-ONE-WAY-DOOR-1`.
