//! The [`Budget`] aggregate — versioned budget config (`SPEC §4.1`).
//!
//! Budgets are versioned by an effective date range, mirroring how the
//! spreadsheet evolved ("2022 Budget" -> "NYC Budget" -> "Polish Budget"). A
//! [`crate::month::Month`] REFERENCES the version active for it by FK (not a
//! copy), so past months keep their referenced version and history stays
//! accurate. `effective_to == None` means this is the current active version.

use chrono::{DateTime, NaiveDate, Utc};

use crate::ids::{BudgetId, UserId};

/// A versioned budget configuration record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Budget {
    /// Stable identity for this specific version.
    pub id: BudgetId,
    /// Owning user.
    pub user_id: UserId,
    /// Human-readable name (e.g. "NYC Budget"). Free-form, no validation.
    pub name: String,
    /// First date this version is in effect.
    pub effective_from: NaiveDate,
    /// Last date this version is in effect; `None` means the current version.
    pub effective_to: Option<NaiveDate>,
    /// When this version was created (UTC, `DOMAIN-7`).
    pub created_at: DateTime<Utc>,
}

impl Budget {
    /// `true` when this is the current active version (`effective_to` is unset).
    #[must_use]
    pub const fn is_current(&self) -> bool {
        self.effective_to.is_none()
    }
}
