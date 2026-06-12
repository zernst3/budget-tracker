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
pub mod config;
pub mod deficit_financing;
pub mod error;
pub mod expected_expense;
pub mod fund;
pub mod income;
pub mod month_lifecycle;
pub mod onboarding;
pub mod plaid_sync;
pub mod portfolio_drip;
pub mod portfolio_review;
pub mod portfolio_snapshot;
pub mod triage;

pub use auth::AuthService;
pub use config::DeficitFinancingConfig;
pub use deficit_financing::{DeficitFinancingOffer, DeficitFinancingService};
pub use error::ServiceError;
pub use expected_expense::ExpectedExpenseService;
pub use fund::{BufferHealth, FundService, LargePurchaseResolution};
pub use income::{
    ConfigDrivenIncomeExpectation, FixedExpectation, IncomeExpectation, IncomeSurplusRouter,
    IncomeSurplusRouting, SemimonthlyFixedExpectation, UnwiredIncomeStub,
};
pub use month_lifecycle::{MonthLifecycleService, net_leftover};
pub use onboarding::{
    BufferOpeningBalance, CategoryOpeningCharge, OnboardingInput, OnboardingReport,
    OnboardingService, opening_charge_id, opening_other_id,
};
pub use plaid_sync::PlaidSyncService;
pub use portfolio_drip::config::DripConfig;
pub use portfolio_drip::{
    ComputedApplication, DripCatchUpResult, DripCatchUpService, PayDatePriceSource,
    compute_accretion, provenance_for,
};
pub use portfolio_review::GeneratePortfolioReview;
pub use portfolio_review::reconcile::{
    MONEY_BAND, PERCENT_PRECISION_DP, ReconcileResult, reconcile,
};
pub use portfolio_snapshot::{assemble_snapshot, assemble_snapshot_with_drip, price_position};
pub use triage::{PendingTransaction, Treatment, TriageInput, TriageOutcome, TriageService};
