//! The [`PaycheckConfig`] aggregate — income setup (`SPEC §4.8`).
//!
//! One per user; extension-ready so a future pay change needs no migration
//! ("design-complete, build-what-you-use"). All mode fields are first-class;
//! only the semimonthly fixed `per_paycheck` path is actively built in V1.
//! Money fields use [`Money`] (`BUDGET-MONEY-1`).

use chrono::NaiveDate;

use crate::enums::{IncomeMode, PaycheckType, SurplusRouting};
use crate::ids::{PaycheckConfigId, UserId};
use crate::money::Money;

/// A user's income configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaycheckConfig {
    /// Stable identity.
    pub id: PaycheckConfigId,
    /// Owning user (one config per user).
    pub user_id: UserId,
    /// How expected monthly income is computed (`per_paycheck` default).
    pub income_mode: IncomeMode,
    /// Paycheck cadence.
    pub paycheck_type: PaycheckType,
    /// Per-paycheck amount; `None` for hourly/variable (degrades to actual-tracking).
    pub amount: Option<Money>,
    /// Next or last paycheck date; the app infers paychecks-per-month from
    /// cadence + this anchor.
    pub anchor_date: NaiveDate,
    /// Default routing for over-expected income (per-transaction override also
    /// supported at the transaction layer).
    pub surplus_routing: SurplusRouting,
    /// Income smoothing buffer for `smoothed` mode; dormant for the semimonthly
    /// case (always 2 paychecks/month, nothing to smooth).
    pub smoothing_buffer: Money,
}
