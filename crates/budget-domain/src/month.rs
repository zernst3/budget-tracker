//! The [`Month`] aggregate — a budget month with a lifecycle (`SPEC §4.6`).
//!
//! Months are DB items (`open` / `closed`) referencing a budget version by FK
//! (not a copy, `SPEC §4.1`). Month-membership is computed in the fixed home
//! timezone `America/New_York`; all timestamps are stored UTC (`D2`,
//! `ARCH-UTC-TIMESTAMPS-1`). Lazy-init creates missing months in order and is
//! idempotent (`BUDGET-IDEMPOTENT-MONTH-INIT-1`); the DB `UNIQUE(user_id, year,
//! month)` is what makes re-entry a no-op.

use chrono::{DateTime, Utc};

use crate::enums::MonthStatus;
use crate::ids::{BudgetId, MonthId, UserId};

/// A budget month.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Month {
    /// Stable identity.
    pub id: MonthId,
    /// Owning user.
    pub user_id: UserId,
    /// The budget version active for this month (referenced, not copied).
    pub budget_id: BudgetId,
    /// Calendar year (e.g. 2026), computed in `America/New_York`.
    pub year: i32,
    /// Calendar month, 1–12, computed in `America/New_York`.
    pub month: i32,
    /// Lifecycle status.
    pub status: MonthStatus,
    /// When the month was opened (UTC, `DOMAIN-7`).
    pub opened_at: DateTime<Utc>,
    /// When the month was closed (UTC, `DOMAIN-7`); `None` while open.
    pub closed_at: Option<DateTime<Utc>>,
}

impl Month {
    /// `true` when the month is still open.
    #[must_use]
    pub const fn is_open(&self) -> bool {
        matches!(self.status, MonthStatus::Open)
    }

    /// A sortable `(year, month)` key for chronological ordering during
    /// multi-month lazy-init catch-up (`BUDGET-IDEMPOTENT-MONTH-INIT-1`).
    #[must_use]
    pub const fn sort_key(&self) -> (i32, i32) {
        (self.year, self.month)
    }
}
