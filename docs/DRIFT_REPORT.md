# Drift Report — budget-tracker

**Overall health: strong.** The build is green (470 tests, 0 failures, 0 clippy warnings, fmt clean), the money type, layering, auth gate, Plaid token vaulting, and rollover idempotency invariants all hold. The findings below are a small number of real latent traps plus some cleanup — nothing is actively corrupting data in a wired production path today.

This report consolidates findings from four parallel scanners (rule-conformance, layer-integrity, code-health, build-test). Duplicates have been merged and several over-stated findings downgraded after reading the code (see notes).

---

## MUST-FIX

### 1. Zero-income seam makes `ensure_month` compute inflated rollovers
- **File:** `crates/budget-ui/src/server_state.rs:223-226`
- **Problem:** `MonthViewState::from_connections` wires the income expectation as `SemimonthlyFixedExpectation::new(Money::ZERO)`. Verified end-to-end: `ensure_month` (a write path, `services/month_view.rs:30`) calls `lifecycle.ensure_current_month`, which calls `prior_month_net → month_net_for`, which applies the D5 formula `net = (actual_income - expected_income) + expense_remaining` (`month_lifecycle.rs:142-146, 268, 406-413`). With `expected_income = ZERO`, any month carrying actual income deposits rolls over inflated by the full income amount instead of only the surplus above expected. This violates BUDGET-ROLLOVER-INTEGRITY-1. The doc comment at `server_state.rs:176-181` acknowledges the seam but frames it as a benign placeholder — it does not flag that the current wiring produces *wrong* rollover numbers the moment an income row exists.
- **Fix:** Either (a) wire the real `ConfigDrivenIncomeExpectation` from `paycheck_config`, or (b) if income rows genuinely cannot exist in the current phase, make `ensure_current_month` assert/guard that `actual_income == 0` before committing a rollover, and update the doc comment at `server_state.rs:176-181` to state the seam is unsafe once income exists. Do not leave it silently wrong.

### 2. `MONTH_NET_SQL` omits `is_fund_draw` (and income) exclusions that the domain predicate applies
- **File:** `crates/budget-infrastructure/src/repositories/transactions.rs:126-131`
- **Problem:** `CATEGORY_SPENT_SQL` (line 101-109) correctly filters `AND is_fund_draw = false`; `MONTH_NET_SQL` does **not**, and also has no income exclusion. The authoritative domain predicate `counts_in_month_expense_remaining` excludes both. A categorized fund draw (positive amount) would be summed into the net, double-counting money already expensed at contribution time (BUDGET-FUND-EARMARK-1 / BUDGET-NO-DOUBLE-CHARGE-1); income rows would inflate net and corrupt D5. Three of four scanners independently flagged this.
- **Mitigating context (why this is must-fix but not "production is broken"):** `TransactionRepository::month_net` is **not** called by any production path today — production rollover routes through `MonthLifecycleService::month_net_for`, which fetches all rows and applies the full Rust predicate. But `month_net` is a public trait method, is tested, and is the obvious thing a future month-close / deficit-financing caller wires up. The existing test `month_net_sql_excludes_transfers` (line ~364) only asserts `is_transfer`/`matched_transaction_id`, so the gap passes CI.
- **Fix:** Add `AND is_fund_draw = false` and the income exclusion (mirror the exact predicate in `counts_in_month_expense_remaining`) to `MONTH_NET_SQL`, OR delete the `month_net` method/trait surface entirely if `month_net_for` is the permanent single path (see SHOULD-FIX 4). Either way, add a behavioral test seeding a fund-draw row and asserting it is excluded. Also fix the misleading doc on the method that calls it "the rolling-Other input (SPEC §4.3)" — production does not use it.

---

## SHOULD-FIX

### 3. Inline ledger edit applies optimistically and swallows the server error with no rollback
- **File:** `crates/budget-ui/src/views/ledger.rs:716-720`
- **Problem:** `table.update_row(...)` applies the edit optimistically (716), then the server call is `let _ = update_transaction_inline(req).await;` (719) — any `Err` is discarded. On a 400/500 the UI shows a category the DB never accepted, with no feedback and no revert. Violates ARCH-STRUCTURED-ERRORS-1.
- **Fix:** Capture the result; on `Err`, revert the row (re-apply the pre-edit value) and surface an error to the user. At minimum, log and revert.

### 4. Empty `transaction_id` sent when row id is missing from `id_map`
- **File:** `crates/budget-ui/src/views/ledger.rs:689`
- **Problem:** `id_map.read().get(&edit.row_id).cloned().unwrap_or_default()` yields `""` when the id is missing, which is then sent as `InlineEditRequest::transaction_id`. The server fails UUID parse (400), and that error is swallowed by finding 3 — a silent no-op that looks like success.
- **Fix:** Replace `unwrap_or_default()` with an early `return` (or `let Some(txn_id) = ... else { return };`) so a missing id never produces a malformed request.

### 5. `month_net` is parallel net-computation logic that exists only for test fakes
- **Files:** `crates/budget-domain/src/repositories.rs:327` (trait), `crates/budget-infrastructure/src/repositories/transactions.rs` (impl ~290), test fake `crates/budget-ui/src/services/ledger_edit.rs:383-426`
- **Problem:** All production net/rollover computation goes through `MonthLifecycleService::month_net_for`. The `month_net` repo method is dead production surface that maintains a *second*, subtly-different net formula (the source of finding 2's drift) plus a test fake (`ledger_edit.rs`) whose `month_net`/`category_spent_for_month` impls also omit `is_fund_draw`/`is_transfer`/income filters. Parallel logic that can drift (SPIRIT-ROBUSTNESS-1).
- **Fix:** Prefer removing the `month_net` trait method and infra impl entirely, collapsing to the single `month_net_for` path. If it must stay, make the SQL, the infra impl, and the `ledger_edit.rs` fake all mirror `counts_in_month_expense_remaining` exactly, and add a doc note that the fakes model the real invariants (ORCH-NEW-PATH-TESTS-1).

### 6. Infrastructure wiring lives in `budget-ui` instead of `budget-server`
- **File:** `crates/budget-ui/src/server_state.rs` — the `from_connections` factory bodies on `AppState`, `MonthViewState`, `TriageState` (e.g. lines 202-246).
- **Problem:** `budget-ui` directly imports and instantiates concrete infra types (`PostgresMonthRepository`, `Argon2idHasher`, `AzureKeyVault`, `HttpPlaidApi`, `SeaOrmUowProvider`, etc.) and takes `sea_orm::DatabaseConnection` in public signatures. The canonical layering is `... ← infrastructure ← app_services ← server`; `budget-server/main.rs` has become a thin pass-through. The whole module is `#[cfg(feature = "server")]`-gated so the wasm build is clean — this is a crate-topology issue, not a binary-size one (RUST-ENTITIES-13 / ARCH-STRICT-LAYERING-1).
- **Fix:** Move the `from_connections` constructor *bodies* (the infra wiring + `DatabaseConnection` params) into `budget-server` (a `wiring.rs` or `main.rs`). Keep the `new(...)` constructors that accept assembled `Arc<dyn Trait>` objects where they are — those are correct. This also removes `sea_orm::DatabaseConnection` from `budget-ui`'s public type surface.

### 7. `triage_transaction` double-fetches the inbox to confirm ownership
- **File:** `crates/budget-ui/src/services/triage.rs:366-377`
- **Problem:** The ownership check loads the *entire* `pending_inbox(user.id())` and scans it for the target id, then `triage(...)` re-fetches the same row via `find_by_id` and re-validates settled/uncategorized/not-transfer (the same guards). An N+2 pattern that scans all uncategorized rows to confirm one id (SQL-DB-NPLUSONE-1 / ARCH-PARALLEL-INDEPENDENT-1). Single-user today, so impact is low.
- **Fix:** Replace the `pending_inbox` scan with a single user-scoped `find_by_id` (add a `user_id` filter to the lookup) for the ownership check, or rely on `triage` performing a user-scoped load and drop the separate pre-check.

### 8. `WebauthnService` reached into directly from the UI layer
- **Files:** `crates/budget-ui/src/services/passkey.rs:128,162`; `crates/budget-ui/src/server_state.rs:68`
- **Problem:** A server fn calls the concrete infra type `WebauthnService::to_domain_credential(...)` directly, and `AppState` holds `Arc<WebauthnService>` rather than `Arc<dyn Trait>` — the only collaborator not behind a domain trait. Server functions should call the app-services boundary, not infrastructure (ARCH-STRICT-LAYERING-1, RUST-DIOXUS-9).
- **Fix:** Add a `register_passkey` method to `AuthService` (in `budget-app-services`) that performs the domain-credential construction and `save_credential`; declare a `WebauthnEngine` trait for the JSON ceremony methods and hold `Arc<dyn WebauthnEngine>` in `AppState`. The server fn then never imports `budget_infrastructure`.

### 9. `.cargo/config.toml` `[paths]` override will become a hard Cargo error
- **File:** `.cargo/config.toml`
- **Problem:** The `paths = [...]` override for `chorale-*` alters the dependency graph, producing the documented "buggy behavior / spurious recompiles" warning on every build. It is a gitignored, documented local-dev workaround, but `[paths]` graph changes are slated to become a hard error in a future Cargo.
- **Fix:** Convert to `[patch.crates-io]` / `[patch."https://..."]` in `Cargo.toml` before the worktree is removed.

---

## NICE-TO-HAVE

### 10. `fund_category_ids` is a dead parameter on `counts_in_month_expense_remaining`
- **File:** `crates/budget-domain/src/predicates.rs:132-155` (the `let _ = fund_category_ids;` no-op at ~141)
- **Problem:** Every call site looks up and supplies a `&[CategoryId]` slice that the predicate ignores (intentional under D6 Model A). A future dev may assume empty-vs-populated changes behavior — a latent reasoning trap.
- **Fix:** Either remove the parameter from the signature (honest, breaking all call sites) or expand the doc comment to state explicitly that it is currently a no-op reserved for a future D6 change.

### 11. `$0.00` settled charge indistinguishable from "no charge" in the settlement proxy
- **File:** `crates/budget-domain/src/predicates.rs:205-239` (`envelope_category_spent`, `counting_sum == Money::ZERO` at ~231)
- **Problem:** A genuine $0.00 settled charge (e.g. a waived fee) is treated as unsettled. The doc acknowledges this and points to the "mark settled" override. Not a codified-rule violation, but a SPEC edge case worth resolving if $0.00 settled charges are allowed.
- **Fix:** SPEC decision — if $0.00 settled charges are in scope, carry an explicit settled flag rather than inferring from a zero sum.

### 12. Behavioral coverage gaps (acknowledged, infra-gated)
- **Files:** `crates/budget-infrastructure/src/repositories/transactions.rs:349-361` (fund-draw exclusion asserted via SQL-text match, not DB behavior); `crates/budget-app-services/tests/month_lifecycle_independent.rs:176` and `onboarding_independent.rs:192` (concurrent month-init race covered only by `#[ignore]`-tagged live-Postgres tests).
- **Problem:** Coverage gaps, not defects. The concurrency ignores are legitimately tagged and gate on a real Postgres in CI.
- **Fix:** Add a mock/live-DB behavioral test seeding `is_fund_draw=true` and asserting exclusion from `category_spent`; stand up a Postgres-in-CI job to un-ignore the two race tests when feasible.

---

## Notes on findings that were downgraded or dropped

- **f64 fields on `DayRow`/`TxnRow` (`ledger.rs:78,97`) — NOT a BUDGET-MONEY-1 violation.** The single conversion site `money_to_cell_f64` (line 109-117) and its doc establish the f64 is display-only for chorale `CellValue::Float`/`Sum` and is **never read back into budget math** — the exact `Money`/`Decimal` stays authoritative server-side. The rule-conformance scanner's "confirm chorale Sum is presentational" is the right instinct but the code already documents it as such; flagging as a violation would be a false positive. Kept out of the action list.
- **`i32::try_from(local.month()).unwrap_or(1)` (`month_lifecycle.rs:498`) — dead branch, low value.** `month()` is always 1..=12, so the fallback is unreachable; cosmetic only. Not worth a tracked item beyond a future `as i32` simplification.
- **`paycheck_config` nullable `amount` and missing composite indexes — no confirmed violation.** The unique index on `user_id` covers the only non-PK access; the deferred-composite-index note is consistent with SQL-DB-INDEX-2 given the single-user bounded-months data model.
