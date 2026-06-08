//! The [`User`] aggregate (`SPEC §5`, `§9`).
//!
//! Single-user in V1 (`SPEC §9`) but every table is `user_id`-shaped for
//! future-proofing. The defining field beyond identity is
//! [`User::tracking_start_date`] — the genesis cutover (`BUDGET-CUTOVER-1`):
//! everything dated before it is CLOSED and lives only in the onboarding opening
//! snapshot; Plaid never ingests pre-this-date transactions.

use chrono::{DateTime, NaiveDate, Utc};

use crate::ids::UserId;
use crate::validated::Email;

/// A registered user (V1: the single user, Zach).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct User {
    /// Stable identity.
    pub id: UserId,
    /// Login identity (validated, `DOMAIN-3`).
    pub email: Email,
    /// Argon2/bcrypt-style password hash (opaque free-form string; never the
    /// plaintext). Kept as `String` because it carries no domain validation.
    pub password_hash: String,
    /// TOTP shared secret, when 2FA is enrolled (`SPEC §9`).
    pub totp_secret: Option<String>,
    /// Genesis cutover date (`BUDGET-CUTOVER-1`). Everything before it is CLOSED;
    /// Plaid sync clamps its lower bound to `max(today − 30d, tracking_start_date)`.
    pub tracking_start_date: NaiveDate,
    /// When the user record was created (UTC, `DOMAIN-7`).
    pub created_at: DateTime<Utc>,
}
