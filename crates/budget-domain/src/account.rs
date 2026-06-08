//! The [`Account`] aggregate — a bank account (`SPEC §3`, `§5`, `§6`).
//!
//! Tracks `BoA` checking / credit card etc., linked via Plaid or entered
//! manually. `plaid_item_id` is optional — an account may exist before being
//! linked, or be manually tracked forever.

use crate::enums::AccountType;
use crate::ids::{AccountId, PlaidItemId, UserId};

/// A bank account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    /// Stable identity.
    pub id: AccountId,
    /// Owning user.
    pub user_id: UserId,
    /// Display name. Free-form, no validation.
    pub name: String,
    /// Account type (`SPEC §5`).
    pub account_type: AccountType,
    /// Plaid-side stable account identifier; `None` for manually-tracked accounts.
    pub plaid_account_id: Option<String>,
    /// FK to the institution link; `None` for manually-tracked accounts.
    pub plaid_item_id: Option<PlaidItemId>,
}
