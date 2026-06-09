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

pub mod auth;
pub mod error;
pub mod fund;
pub mod income;
pub mod month_lifecycle;
pub mod onboarding;
pub mod plaid_sync;

pub use auth::AuthService;
pub use error::ServiceError;
pub use fund::{BufferHealth, FundService, LargePurchaseResolution};
pub use income::{
    ConfigDrivenIncomeExpectation, FixedExpectation, IncomeExpectation, IncomeSurplusRouter,
    IncomeSurplusRouting, SemimonthlyFixedExpectation,
};
pub use month_lifecycle::{MonthLifecycleService, net_leftover};
pub use onboarding::{
    BufferOpeningBalance, CategoryOpeningCharge, OnboardingInput, OnboardingReport,
    OnboardingService, opening_charge_id, opening_other_id,
};
pub use plaid_sync::PlaidSyncService;
