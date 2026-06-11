//! The app-services error enum (`RUST-DOMAIN-4` / `RUST-DOMAIN-6`).
//!
//! Services orchestrate over the domain + repositories. Most failures are
//! domain-rule or persistence failures, which already have a typed home in
//! [`budget_domain::error::DomainError`]; [`ServiceError`] wraps that
//! ([`ServiceError::Domain`]) so existing call sites keep their error shape.
//!
//! The new failure surface step 6 introduces is **unsupported income
//! configuration**. The income schema is design-complete for every mode and
//! cadence (`SPEC §4.8`, "design-complete, build-what-you-use",
//! `SPIRIT-ROBUSTNESS-1`), but only the semimonthly fixed `per_paycheck` path
//! (Zach's actual mode) and the hourly/variable degradation are *built*. The
//! stubbed paths (biweekly / weekly cadence resolution, the `smoothed`
//! 12-month-average mode, and the income smoothing buffer) must fail
//! **loudly-but-safely** rather than silently miscompute or `panic!`/`todo!()`
//! on a reachable path — so they surface as the typed
//! [`ServiceError::UnsupportedIncome`] at the (fallible, async) construction
//! boundary where an income engine is built from a persisted
//! [`budget_domain::paycheck_config::PaycheckConfig`]. See
//! [`crate::income::ConfigDrivenIncomeExpectation`].

use thiserror::Error;

use budget_domain::enums::{IncomeMode, PaycheckType};
use budget_domain::error::DomainError;

/// The single shared app-services error (`RUST-DOMAIN-6`).
///
/// One enum per failure category at the crate boundary: domain/persistence
/// failures flow through [`ServiceError::Domain`]; the income engine's
/// not-yet-built configurations surface as [`ServiceError::UnsupportedIncome`].
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ServiceError {
    /// A domain-rule or persistence failure (wraps [`DomainError`], which in turn
    /// wraps [`budget_domain::error::RepositoryError`] /
    /// [`budget_domain::error::ValidationError`]).
    #[error(transparent)]
    Domain(#[from] DomainError),

    /// The user's income configuration selects a mode/cadence that is
    /// design-complete but **not built** in V1 (`SPEC §4.8`,
    /// `SPIRIT-ROBUSTNESS-1`). Built paths: `per_paycheck` + `semimonthly`
    /// (`amount` set), and any cadence with a blank `amount` (the hourly/variable
    /// degradation, which returns a zero expectation). Everything else — the
    /// `smoothed` mode, biweekly/weekly cadence resolution, and the income
    /// smoothing buffer — fails here loudly-but-safely rather than silently
    /// miscomputing.
    #[error(
        "unsupported income configuration (mode={mode:?}, cadence={cadence:?}): \
         only per_paycheck + semimonthly (with an amount) and the blank-amount \
         hourly/variable degradation are built in V1 (SPEC §4.8); {detail}"
    )]
    UnsupportedIncome {
        /// The configured income mode.
        mode: IncomeMode,
        /// The configured paycheck cadence.
        cadence: PaycheckType,
        /// Which specific unbuilt path was selected.
        detail: &'static str,
    },

    /// A retryable transport failure from the [`InvestmentAdvisor`] port that is
    /// NOT a parse failure (`docs/AI_FEATURE_DESIGN.md §Phase 5`): an `Api`,
    /// `RateLimited`, `Unavailable`, or `SecretVault` advisor error. The
    /// portfolio-review use-case maps every non-`Parse` advisor error here and
    /// does NOT persist a `ReviewRun` (the call can be retried); a `Parse` failure
    /// instead persists a `MalformedOutput` run with the raw output. Carries only
    /// a `String` — never the API key or an HTTP status (`§0.3`).
    ///
    /// [`InvestmentAdvisor`]: budget_domain::portfolio::InvestmentAdvisor
    #[error("portfolio advisor transport failure: {0}")]
    AdvisorTransport(String),
}

impl From<budget_domain::error::RepositoryError> for ServiceError {
    fn from(e: budget_domain::error::RepositoryError) -> Self {
        ServiceError::Domain(DomainError::Repository(e))
    }
}
