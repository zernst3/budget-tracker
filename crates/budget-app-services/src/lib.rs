//! Use-case orchestration for the budget tracker.
//!
//! Wires domain operations together: the month lifecycle + lazy idempotent
//! init (`BUDGET-IDEMPOTENT-MONTH-INIT-1`), the rolling Other balance
//! (`BUDGET-ROLLOVER-INTEGRITY-1`), funds + large purchases
//! (`BUDGET-FUND-EARMARK-1`), income (per-paycheck built, smoothed stubbed),
//! and Plaid sync. Depends only on `budget-domain`. Invoked directly by the
//! Dioxus server functions (`D1`: server functions -> services -> repositories).
//!
//! Services land in build step 3+ (see `.build-progress.md`).
