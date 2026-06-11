//! The income engine (`SPEC §4.8`, `D5`) — step 6.
//!
//! The month net-leftover formula (`D5`, `BUDGET-ROLLOVER-INTEGRITY-1`) is
//!
//! ```text
//! net = (actual_income - expected_income) + Σ(expense category remaining)
//! ```
//!
//! Computing **`expected_income`** for a `(year, month)` is the whole of income
//! step 6. The month-lifecycle service (build step 4) depends only on the
//! [`IncomeExpectation`] trait — the *figure*, not the machinery — so nothing in
//! the lifecycle changes when the engine behind the seam grows.
//!
//! ## What is BUILT vs STUBBED (`SPEC §4.8`, "design-complete, build-what-you-use", `SPIRIT-ROBUSTNESS-1`)
//!
//! The schema (`budget_domain::paycheck_config::PaycheckConfig` + the
//! [`budget_domain::enums::IncomeMode`] / [`budget_domain::enums::PaycheckType`]
//! / [`budget_domain::enums::SurplusRouting`] enums) is design-complete for every
//! mode and cadence, so a future pay change needs no migration. Only the paths
//! Zach is actually on are *built*:
//!
//! - **BUILT — `per_paycheck` + `semimonthly` (with an `amount`).** Semimonthly
//!   is always exactly 2 paychecks/month (`SPEC §4.8`, "Zach's own situation"), so
//!   expected income is `2 × amount` every month, independent of the anchor — no
//!   cadence arithmetic, no buffer, both [`IncomeMode`] values identical.
//! - **BUILT — the hourly/variable degradation (`amount = None`).** Any cadence
//!   with a blank amount degrades to [`Money::ZERO`] expected, so the net becomes
//!   `actual - 0 = actual` and the income flows straight into Other (`SPEC §4.8`).
//! - **STUBBED — biweekly / weekly cadence resolution** (26/52 per year,
//!   2–3 / 4–5 per month, needing anchor-date arithmetic),
//! - **STUBBED — the `smoothed` 12-month-average mode** (and its income smoothing
//!   buffer).
//!
//! The stubbed paths are NOT `todo!()`/`panic!`: they fail
//! **loudly-but-safely** as the typed [`crate::error::ServiceError::UnsupportedIncome`]
//! at the (fallible, async) construction boundary
//! [`ConfigDrivenIncomeExpectation::load`], so the `match` arms over the full mode
//! matrix are complete and exhaustive but the unbuilt paths cannot silently
//! miscompute.
//!
//! ## Trait signature is UNCHANGED (`SPEC §4.8` build-discipline; `ORCH-ONE-WAY-DOOR-1`)
//!
//! The seam's [`IncomeExpectation::expected_income`] stays
//! `fn(user, year, month) -> Money` — sync and infallible. The config-driven
//! engine reads the persisted [`budget_domain::paycheck_config::PaycheckConfig`]
//! **once, at construction** (the async, fallible boundary), validates that the
//! selected mode/cadence is built, and stores the resolved per-month figure; the
//! sync `expected_income` then computes purely from that stored figure. This is
//! how a DB-driven expectation lives behind an unchanged sync infallible trait
//! without widening it.
//!
//! ## Surplus routing (`SPEC §4.8`) — [`IncomeSurplusRouter`]
//!
//! When an actual deposit exceeds expected, the over-amount is routed by the
//! `surplus_routing` default plus a per-transaction override. `this_month` leaves
//! the surplus raising Other (the `D5` formula already does this, so it is a
//! no-op); `buffer` / `savings` route the surplus into the respective fund,
//! reusing the step-5 [`crate::fund::FundService::contribute`] plumbing
//! (`BUDGET-FUND-EARMARK-1` / D6 Model A). In `per_paycheck` mode the surplus
//! auto-raises Other by formula with no routing decision (`D5`), so routing is the
//! smoothed-mode extension; the per-transaction override is what lets a
//! higher-than-budgeted paycheck be added to *this* month deliberately.

use std::sync::Arc;

use budget_domain::enums::{IncomeMode, PaycheckType, SurplusRouting};
use budget_domain::ids::{CategoryId, FundId, MonthId, UserId};
use budget_domain::money::Money;
use budget_domain::paycheck_config::PaycheckConfig;
use budget_domain::repositories::PaycheckConfigRepository;

use chrono::{DateTime, NaiveDate, Utc};

use crate::error::ServiceError;
use crate::fund::FundService;

/// The expected (budgeted) income for a user in a given calendar month.
///
/// This is the `expected_income` term of the `D5` net formula and the single
/// seam between the month-lifecycle service (build step 4) and the income engine
/// (this step): the lifecycle service depends on this trait, never on a concrete
/// income mode.
///
/// The signature is intentionally **sync + infallible** — `(user, year, month)
/// -> Money` — and step 6 keeps it that way (`SPEC §4.8` build-discipline;
/// `ORCH-ONE-WAY-DOOR-1`). Implementors that need DB-backed config resolve it at
/// construction time (see [`ConfigDrivenIncomeExpectation`]). Implementors that
/// cannot form an expectation (hourly/variable income with a blank amount,
/// `SPEC §4.8`) return [`Money::ZERO`], which degrades the net to pure
/// actual-income tracking — `actual - 0 = actual` flows straight into Other.
///
/// Month-membership of paychecks is the implementor's concern and is computed in
/// the fixed home timezone (`America/New_York`, `D2`) consistent with the rest of
/// the lifecycle service.
pub trait IncomeExpectation: Send + Sync {
    /// The expected income for `user` in the calendar `(year, month)`.
    ///
    /// Returns a signed [`Money`] in the internal convention (income is
    /// positive).
    fn expected_income(&self, user: UserId, year: i32, month: i32) -> Money;

    /// Whether this expectation is wired to a real income source and may
    /// therefore be safely subtracted from `actual_income` in the `D5` rollover
    /// formula (`BUDGET-ROLLOVER-INTEGRITY-1`).
    ///
    /// Defaults to `true`: a real `SemimonthlyFixedExpectation`,
    /// `FixedExpectation`, or `ConfigDrivenIncomeExpectation` is always
    /// trustworthy — its zero (the hourly/variable degradation, `SPEC §4.8`) is
    /// a *correct* expectation, not an absence of one.
    ///
    /// The one exception is [`UnwiredIncomeStub`], the placeholder wired into the
    /// read-only month view before real income wiring (`B4`) lands. It overrides
    /// this to `false` so the month-lifecycle rollover-commit path can FAIL LOUD
    /// (`SPIRIT-ROBUSTNESS-1`) rather than commit a rollover inflated by the full
    /// (un-subtracted) income amount the moment a real income row appears. See
    /// [`crate::month_lifecycle::MonthLifecycleService::prior_month_net`].
    fn is_trustworthy(&self) -> bool {
        true
    }
}

/// The number of semimonthly paychecks in any month — always two (`SPEC §4.8`).
/// A named constant so the "2" is not a bare magic number at the call sites.
const SEMIMONTHLY_PAYCHECKS_PER_MONTH: i64 = 2;

/// The config-driven income expectation (`SPEC §4.8`) — step 6's real engine.
///
/// Built from the user's persisted [`PaycheckConfig`] (read via the
/// [`PaycheckConfigRepository`] — `db.*` stays in repositories,
/// `ARCH-STRICT-LAYERING-1`), so the month math uses the user's *real*
/// cadence/mode rather than a hardcoded constructor.
///
/// The persisted config is resolved into a flat per-month figure **once, at
/// construction** ([`Self::load`]), which is the only place the
/// not-yet-built mode/cadence matrix can be rejected
/// ([`ServiceError::UnsupportedIncome`]). After that, [`Self::expected_income`]
/// is pure: it returns the stored figure for every month (true for both built
/// paths — semimonthly is the same `2 × amount` each month, and the
/// hourly/variable degradation is [`Money::ZERO`] each month).
#[derive(Debug, Clone, Copy)]
pub struct ConfigDrivenIncomeExpectation {
    /// The user this expectation is for (the trait passes a `user` argument; we
    /// assert it matches so a misrouted call cannot silently return the wrong
    /// user's figure).
    user_id: UserId,
    /// The flat per-month expected income, pre-resolved from the config.
    per_month: Money,
}

impl ConfigDrivenIncomeExpectation {
    /// Build the expectation from the user's persisted income config
    /// (`SPEC §4.8`).
    ///
    /// Reads the [`PaycheckConfig`] via the repository, then resolves the
    /// per-month expectation for the **built** paths and rejects every **stubbed**
    /// path with [`ServiceError::UnsupportedIncome`]:
    ///
    /// - `amount = None` (hourly/variable, any cadence/mode) -> [`Money::ZERO`]
    ///   (the actual-tracking degradation, `SPEC §4.8`). This is checked first
    ///   because a blank amount is buildable regardless of the nominal cadence.
    /// - `per_paycheck` + `semimonthly` + `Some(amount)` -> `2 × amount`.
    /// - `smoothed` (any cadence) -> rejected (the buffer is unbuilt).
    /// - `per_paycheck` + `biweekly`/`weekly` + `Some(amount)` -> rejected
    ///   (anchor-date cadence resolution is unbuilt).
    /// - `per_paycheck` + `hourly` + `Some(amount)` -> rejected (an hourly config
    ///   with a fixed amount is contradictory; hourly is the blank-amount path).
    ///
    /// # Errors
    /// - [`ServiceError::Domain`] if the user has no income config or on any
    ///   persistence failure.
    /// - [`ServiceError::UnsupportedIncome`] if the config selects a
    ///   design-complete-but-unbuilt mode/cadence (`SPEC §4.8`).
    pub async fn load(
        repo: &dyn PaycheckConfigRepository,
        user_id: UserId,
    ) -> Result<Self, ServiceError> {
        let config = repo.find_for_user(user_id).await?.ok_or_else(|| {
            ServiceError::Domain(budget_domain::error::DomainError::Invariant(format!(
                "no paycheck config for user {user_id}"
            )))
        })?;
        let per_month = resolve_per_month(&config)?;
        Ok(Self { user_id, per_month })
    }

    /// Build directly from an already-loaded [`PaycheckConfig`] (`SPEC §4.8`).
    ///
    /// The synchronous core of [`Self::load`], factored out so callers that
    /// already hold the config (and tests) need not go through the repository.
    /// Same built/stubbed resolution and same loud-but-safe rejection.
    ///
    /// # Errors
    /// [`ServiceError::UnsupportedIncome`] for any design-complete-but-unbuilt
    /// mode/cadence (`SPEC §4.8`).
    pub fn from_config(config: &PaycheckConfig) -> Result<Self, ServiceError> {
        let per_month = resolve_per_month(config)?;
        Ok(Self {
            user_id: config.user_id,
            per_month,
        })
    }
}

/// Resolve a [`PaycheckConfig`] into its flat per-month expected income for the
/// **built** paths, rejecting every **stubbed** path loudly-but-safely
/// (`SPEC §4.8`, `SPIRIT-ROBUSTNESS-1`).
///
/// The full mode × cadence matrix is matched exhaustively so the schema and the
/// arms are complete; the unbuilt arms return
/// [`ServiceError::UnsupportedIncome`] rather than `todo!()`/`panic!`.
fn resolve_per_month(config: &PaycheckConfig) -> Result<Money, ServiceError> {
    // BUILT: hourly / variable degradation — a blank amount means no expectation
    // can be formed, so the net degrades to pure actual-tracking (SPEC §4.8).
    // Checked first: a blank amount is buildable irrespective of the nominal
    // cadence/mode.
    let Some(amount) = config.amount else {
        return Ok(Money::ZERO);
    };

    match (config.income_mode, config.paycheck_type) {
        // BUILT: the only fully-built path — semimonthly per-paycheck, always
        // exactly 2 paychecks/month (SPEC §4.8, "Zach's own situation").
        (IncomeMode::PerPaycheck, PaycheckType::Semimonthly) => Ok(semimonthly_expected(amount)),

        // STUBBED: smoothed (12-month-average) mode + its income smoothing
        // buffer — dormant for Zach (always 2 paychecks/month, nothing to
        // smooth), unbuilt for everyone (SPEC §4.8).
        (IncomeMode::Smoothed, cadence) => Err(ServiceError::UnsupportedIncome {
            mode: IncomeMode::Smoothed,
            cadence,
            detail: "the smoothed 12-month-average mode and its income smoothing \
                     buffer are stubbed (design-complete, build-what-you-use)",
        }),

        // STUBBED: per-paycheck biweekly / weekly — needs anchor-date arithmetic
        // to count how many paychecks land in a given month (2–3 biweekly,
        // 4–5 weekly), which is unbuilt (SPEC §4.8).
        (IncomeMode::PerPaycheck, cadence @ (PaycheckType::Biweekly | PaycheckType::Weekly)) => {
            Err(ServiceError::UnsupportedIncome {
                mode: IncomeMode::PerPaycheck,
                cadence,
                detail: "biweekly/weekly per-month paycheck counting (anchor-date \
                         cadence resolution) is stubbed",
            })
        }

        // STUBBED (contradictory): hourly cadence with a fixed amount. The hourly
        // path is the blank-amount degradation handled above; a fixed amount on an
        // hourly cadence is a config error, surfaced loudly rather than guessed.
        (IncomeMode::PerPaycheck, PaycheckType::Hourly) => Err(ServiceError::UnsupportedIncome {
            mode: IncomeMode::PerPaycheck,
            cadence: PaycheckType::Hourly,
            detail: "hourly cadence carries a fixed amount; hourly/variable income \
                     must leave amount blank (the actual-tracking degradation)",
        }),
    }
}

/// Expected income for the semimonthly fixed path: `2 × amount`, every month
/// (`SPEC §4.8`). Summed via [`Money`] arithmetic (`BUDGET-MONEY-1`) so no float
/// or `i64`-cents intermediate is introduced.
#[must_use]
fn semimonthly_expected(per_paycheck_amount: Money) -> Money {
    let mut total = Money::ZERO;
    for _ in 0..SEMIMONTHLY_PAYCHECKS_PER_MONTH {
        total += per_paycheck_amount;
    }
    total
}

impl IncomeExpectation for ConfigDrivenIncomeExpectation {
    fn expected_income(&self, user: UserId, _year: i32, _month: i32) -> Money {
        // The expectation was resolved per-user at construction. If a call is
        // routed for a different user, returning this user's figure would be a
        // silent cross-user leak; return ZERO (the safe degradation) instead. In
        // single-user V1 (SPEC §9) the ids always match, so this is defensive
        // depth, not a live branch.
        if user != self.user_id {
            return Money::ZERO;
        }
        // Both built paths produce the same figure every month (semimonthly =
        // 2 × amount; hourly/variable degradation = ZERO), so the stored figure
        // is the answer for any (year, month). Biweekly/weekly/smoothed — which
        // WOULD vary by month — never reach here: they are rejected at load().
        self.per_month
    }
}

/// The minimal income expectation for the semimonthly fixed mode, constructed
/// directly from an amount (`SPEC §4.8`, "Zach's own situation").
///
/// Retained as a lightweight, config-free constructor for the month-lifecycle
/// tests and any caller that already knows the per-paycheck amount and does not
/// need a repository round-trip. The config-driven engine
/// ([`ConfigDrivenIncomeExpectation`]) is the production path.
#[derive(Debug, Clone, Copy)]
pub struct SemimonthlyFixedExpectation {
    per_paycheck_amount: Money,
}

impl SemimonthlyFixedExpectation {
    /// Build the expectation from the fixed per-paycheck amount.
    #[must_use]
    pub const fn new(per_paycheck_amount: Money) -> Self {
        Self {
            per_paycheck_amount,
        }
    }
}

impl IncomeExpectation for SemimonthlyFixedExpectation {
    fn expected_income(&self, _user: UserId, _year: i32, _month: i32) -> Money {
        semimonthly_expected(self.per_paycheck_amount)
    }
}

/// An income expectation that always returns a fixed, injected value.
///
/// Two uses:
///   - the hourly / variable degradation path (`SPEC §4.8`: blank amount ->
///     [`Money::ZERO`] -> pure actual tracking), and
///   - tests that want to drive the netting with an exact expected figure without
///     reconstructing cadence math.
#[derive(Debug, Clone, Copy)]
pub struct FixedExpectation {
    value: Money,
}

impl FixedExpectation {
    /// Build a fixed expectation returning `value` for every month.
    #[must_use]
    pub const fn new(value: Money) -> Self {
        Self { value }
    }

    /// The zero expectation — the hourly / variable degradation default
    /// (`SPEC §4.8`).
    #[must_use]
    pub const fn zero() -> Self {
        Self { value: Money::ZERO }
    }
}

impl IncomeExpectation for FixedExpectation {
    fn expected_income(&self, _user: UserId, _year: i32, _month: i32) -> Money {
        self.value
    }
}

/// The UNWIRED income placeholder (`SPEC §4.8`, `B4`).
///
/// Wired into the read-only month view (`budget-ui` `MonthViewState`) before the
/// real config-driven income engine ([`ConfigDrivenIncomeExpectation`]) is
/// connected. It returns [`Money::ZERO`] expected income for every month — but,
/// unlike [`FixedExpectation::zero`] (the *legitimate* hourly/variable
/// degradation), it reports [`IncomeExpectation::is_trustworthy`] as `false`.
///
/// The distinction is load-bearing (`SPIRIT-ROBUSTNESS-1`, named threat = a
/// corrupted rollover chain): with a zero expectation, the `D5` formula
/// `net = (actual_income - expected_income) + expense_remaining` rolls a month
/// forward inflated by the *entire* income amount the moment any actual income
/// row exists (`BUDGET-ROLLOVER-INTEGRITY-1`). While the seam is unwired, no
/// production path writes an income row, so the figure is correct *today*; this
/// stub makes the unsafety explicit so the rollover-commit path can FAIL LOUD if
/// that assumption is ever violated, instead of silently committing a wrong
/// rollover. Replace it with [`ConfigDrivenIncomeExpectation`] (`B4`) before any
/// income row is written.
#[derive(Debug, Clone, Copy, Default)]
pub struct UnwiredIncomeStub;

impl UnwiredIncomeStub {
    /// Construct the unwired placeholder.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl IncomeExpectation for UnwiredIncomeStub {
    fn expected_income(&self, _user: UserId, _year: i32, _month: i32) -> Money {
        Money::ZERO
    }

    /// Always `false`: this placeholder is NOT a real income source, so its zero
    /// must never be silently subtracted in a committed rollover.
    fn is_trustworthy(&self) -> bool {
        false
    }
}

/// Where an over-expected income surplus is routed (`SPEC §4.8`).
///
/// The default comes from [`PaycheckConfig::surplus_routing`]; a per-transaction
/// override (Zach's "this paycheck was higher than budgeted, add the extra to
/// THIS month" checkbox) is supplied at routing time. This is the domain enum
/// re-used directly; the router is what acts on it.
pub use budget_domain::enums::SurplusRouting as IncomeSurplusRouting;

/// Routes over-expected income surplus per the `surplus_routing` default plus a
/// per-transaction override (`SPEC §4.8`).
///
/// Reuses the step-5 fund plumbing (`BUDGET-FUND-EARMARK-1` / D6 Model A): a
/// `buffer` / `savings` route is a [`FundService::contribute`] into the
/// respective fund (the surplus becomes a counted Other-bucket expense that
/// raises the fund balance); a `this_month` route is a no-op because the `D5`
/// formula already raises Other by the surplus (no discrete line item).
///
/// In `per_paycheck` mode (Zach's) the surplus auto-raises Other by formula with
/// no routing decision (`D5`), so this router is the smoothed-mode extension; the
/// per-transaction override is the mechanism by which a higher-than-budgeted
/// paycheck is deliberately added to *this* month.
pub struct IncomeSurplusRouter {
    funds: Arc<FundService>,
}

impl IncomeSurplusRouter {
    /// Wire the router from the step-5 fund service (`SERVICE-DI-1`).
    #[must_use]
    pub fn new(funds: Arc<FundService>) -> Self {
        Self { funds }
    }

    /// Resolve the effective routing for one surplus event: the per-transaction
    /// `override_routing` if present, else the config `default_routing`
    /// (`SPEC §4.8`).
    #[must_use]
    pub fn effective_routing(
        default_routing: SurplusRouting,
        override_routing: Option<SurplusRouting>,
    ) -> SurplusRouting {
        override_routing.unwrap_or(default_routing)
    }

    /// Route an income surplus (`SPEC §4.8`).
    ///
    /// `surplus` is the positive over-expected magnitude (`actual - expected`,
    /// already known to be positive by the caller). `target_fund_id` is required
    /// for the `buffer` / `savings` routes (the fund the surplus is contributed
    /// into) and ignored for `this_month`.
    ///
    /// - [`SurplusRouting::ThisMonth`] -> a no-op: the `D5` net already raises
    ///   Other by the surplus, so there is nothing to move.
    /// - [`SurplusRouting::Buffer`] / [`SurplusRouting::Savings`] -> a
    ///   [`FundService::contribute`] of `surplus` into `target_fund_id`, which
    ///   posts a counted Other-bucket expense (`BUDGET-FUND-EARMARK-1`) and raises
    ///   the fund balance by the same amount. (`savings` routes into a fund the
    ///   app tracks; an externally-held savings account is modeled as a
    ///   surplus-kind fund, `SPEC §4.9`.)
    ///
    /// # Errors
    /// - [`ServiceError::Domain`] if `surplus` is not positive, if a
    ///   buffer/savings route is missing its `target_fund_id`, or on any
    ///   persistence/fund failure.
    #[allow(clippy::too_many_arguments)]
    pub async fn route_surplus(
        &self,
        routing: SurplusRouting,
        surplus: Money,
        target_fund_id: Option<FundId>,
        month_id: MonthId,
        earmark_category_id: CategoryId,
        date: NaiveDate,
        now: DateTime<Utc>,
    ) -> Result<(), ServiceError> {
        if !surplus.is_positive() {
            return Err(ServiceError::Domain(
                budget_domain::error::DomainError::Invariant(
                    "income surplus to route must be positive".to_owned(),
                ),
            ));
        }
        match routing {
            // The D5 formula already raises Other by the surplus; nothing to move.
            SurplusRouting::ThisMonth => Ok(()),
            // Reuse the step-5 contribute plumbing: the surplus becomes a counted
            // Other-bucket expense raising the target fund's balance
            // (BUDGET-FUND-EARMARK-1 / D6 Model A).
            SurplusRouting::Buffer | SurplusRouting::Savings => {
                let fund_id = target_fund_id.ok_or_else(|| {
                    ServiceError::Domain(budget_domain::error::DomainError::Invariant(format!(
                        "surplus routing {routing:?} requires a target fund id"
                    )))
                })?;
                self.funds
                    .contribute(fund_id, month_id, earmark_category_id, surplus, date, now)
                    .await?;
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests;
